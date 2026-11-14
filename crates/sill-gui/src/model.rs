//! Toolkit-free reconstruction of the public SFP view for the frontend spike.
//! Kept separate from rendering so this can move into sill-client after the spike.
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use sill_proto::{Client, sfp::*};

#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub session_id: String,
    pub subscription_id: String,
    pub seq: u64,
    pub session: Option<SessionState>,
    pub items: BTreeMap<String, Item>,
    pub questions: BTreeMap<String, Question>,
    pub submissions: BTreeMap<String, Submission>,
    /// Turn outcomes, for the note a stopped or failed turn ends with.
    pub turns: BTreeMap<String, Turn>,
}

impl Conversation {
    /// Notifications arriving during pagination remain queued on Client. The UI installs
    /// the entire snapshot before applying them, ignoring events through snapshot.at.
    pub fn attach(client: &Client, session_id: &str) -> Result<Self> {
        let attached: Attached = client.call(
            "_sill/sessions.attach",
            Attach {
                session_id: session_id.into(),
                since: None,
            },
        )?;
        let Attached::Snapshot {
            subscription_id,
            snapshot,
        } = attached
        else {
            bail!("expected a snapshot for a fresh attachment");
        };
        ensure!(
            snapshot.at.session_id == session_id,
            "snapshot belongs to another session"
        );
        let mut view = Self {
            session_id: session_id.into(),
            subscription_id,
            seq: snapshot
                .at
                .seq
                .parse()
                .context("invalid snapshot sequence")?,
            ..Self::default()
        };
        let mut decoder = SnapshotDecoder::default();
        let mut page = None;
        loop {
            let part: SnapshotPage = client.call(
                "_sill/sessions.snapshot",
                SnapshotRequest {
                    token: snapshot.token.clone(),
                    page,
                },
            )?;
            ensure!(
                part.at == snapshot.at,
                "snapshot cursor changed during pagination"
            );
            for record in decoder.records(&part).map_err(anyhow::Error::msg)? {
                match record {
                    Record::Session { session } => {
                        ensure!(session.session_id == session_id, "wrong session record");
                        view.session = Some(session);
                    }
                    Record::Item { item } => {
                        view.items.insert(item.item_id.clone(), item);
                    }
                    Record::Question { question } => {
                        view.questions
                            .insert(question.question_id.clone(), question);
                    }
                    Record::Submission { submission } => {
                        view.submissions
                            .insert(submission.submission_id.clone(), submission);
                    }
                    Record::Turn { turn } => {
                        view.turns.insert(turn.turn_id.clone(), turn);
                    }
                }
            }
            page = part.next_page;
            if page.is_none() {
                break;
            }
        }
        ensure!(view.session.is_some(), "snapshot has no session record");
        Ok(view)
    }

    /// False means a foreign subscription or an already applied event. Gaps require a
    /// fresh snapshot; never advance the cursor on failure or display partial history.
    pub fn accept(&mut self, notification: SessionEvent) -> Result<bool> {
        if notification.session_id != self.session_id
            || notification.subscription_id != self.subscription_id
        {
            return Ok(false);
        }
        let seq = notification
            .seq
            .parse::<u64>()
            .context("invalid event sequence")?;
        if seq <= self.seq {
            return Ok(false);
        }
        ensure!(
            self.seq.checked_add(1) == Some(seq),
            "conversation event gap after {}",
            self.seq
        );
        match notification.event {
            Event::ItemCreated { item } => {
                ensure!(
                    !self.items.contains_key(&item.item_id),
                    "duplicate item identity"
                );
                self.items.insert(item.item_id.clone(), item);
            }
            Event::ItemPatched { item_id, set } => {
                let item = self
                    .items
                    .get_mut(&item_id)
                    .context("missing patch target")?;
                if let Some(content) = set.content {
                    item.content = content;
                }
                if let Some(tool) = set.tool {
                    item.tool = Some(tool);
                }
            }
            Event::ItemTextAppended {
                item_id,
                block_id,
                text,
            } => {
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
            }
            Event::ItemRemoved { item_id, .. } => {
                ensure!(
                    self.items.remove(&item_id).is_some(),
                    "missing removal target"
                );
            }
            Event::SessionUpdated { session } => {
                ensure!(
                    session.session_id == self.session_id,
                    "wrong session update"
                );
                self.session = Some(session);
            }
            Event::QuestionUpdated { question } => {
                self.questions
                    .insert(question.question_id.clone(), question);
            }
            Event::SubmissionUpdated { submission } => {
                self.submissions
                    .insert(submission.submission_id.clone(), submission);
            }
            Event::TurnUpdated { turn } => {
                self.turns.insert(turn.turn_id.clone(), turn);
            }
        }
        self.seq = seq;
        Ok(true)
    }

    pub fn visible_items(&self) -> Vec<&Item> {
        let mut items: Vec<_> = self
            .items
            .values()
            .filter(|item| {
                // A queued prompt is not yet part of the agent's conversation.
                !(item.kind == "user"
                    && item
                        .submission_id
                        .as_ref()
                        .and_then(|id| self.submissions.get(id))
                        .is_some_and(|s| s.delivery == "queued"))
            })
            .collect();
        items.sort_by(|a, b| {
            a.created_seq
                .parse::<u64>()
                .unwrap_or_default()
                .cmp(&b.created_seq.parse::<u64>().unwrap_or_default())
                .then_with(|| a.item_id.cmp(&b.item_id))
        });
        items
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

    pub fn transcript(&self) -> String {
        self.visible_items()
            .into_iter()
            .map(|item| format!("{}\n{}", item.kind, item_text(item)))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

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
            parts.push("[Image — viewing uploads is not implemented in this spike]".into());
        }
    }
    parts.join("\n\n")
}
