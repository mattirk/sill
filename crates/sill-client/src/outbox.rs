//! A prompt between the person pressing Send and the daemon having it. It keeps its text and its
//! submission identity until the conversation shows the daemon accepted it, so:
//!
//! - a refusal, or a request that never left, gives the text back instead of losing it;
//! - a lost connection leaves it *uncertain*, never resent on its own. Checking asks the daemon for
//!   the submission by identity; trying again reuses the identity, which the daemon recognises
//!   (an identical retry returns the original), so nothing is sent twice.
use sill_proto::sfp::{Submission, SubmissionKey, Submit};

use crate::Conversation;
use crate::actions::Outcome;

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Sending,
    /// It did not reach the agent; the reason. The text is the person's to edit or send again.
    Failed(String),
    /// It may have reached the daemon; the reason it is not known.
    Uncertain(String),
    /// Asking the daemon whether it has the submission.
    Checking,
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub submit: Submit,
    pub state: State,
}

impl Pending {
    pub fn id(&self) -> &str {
        &self.submit.submission_id
    }
    pub fn text(&self) -> &str {
        &self.submit.text
    }
}

/// What the frontend does after an outcome.
#[derive(Debug, PartialEq)]
pub enum Settled {
    /// The daemon has it; the conversation will show it.
    Accepted,
    /// It failed and is kept, shown with its reason.
    Kept,
    /// It may have arrived; shown as uncertain.
    Uncertain,
    /// Unknown identity (discarded meanwhile).
    Unknown,
}

#[derive(Debug, Default, Clone)]
pub struct Outbox {
    pending: Vec<Pending>,
}

impl Outbox {
    /// A new submission of `text` to `session_id`: the request to make.
    pub fn send(&mut self, session_id: &str, text: String) -> Submit {
        self.send_with_attachments(session_id, text, vec![])
    }

    pub fn send_with_attachments(&mut self, session_id: &str, text: String, attachments: Vec<sill_proto::sfp::Attachment>) -> Submit {
        self.send_for(session_id, text, attachments, None)
    }

    /// A submission with a `purpose` (`Submit::purpose`): a prompt written for the person.
    pub fn send_for(&mut self, session_id: &str, text: String, attachments: Vec<sill_proto::sfp::Attachment>, purpose: Option<String>) -> Submit {
        let submit = Submit { session_id: session_id.into(), submission_id: crate::request_id("prompt"), text, attachments, purpose };
        self.pending.push(Pending { submit: submit.clone(), state: State::Sending });
        submit
    }

    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    pub fn get(&self, id: &str) -> Option<&Pending> {
        self.pending.iter().find(|p| p.id() == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Pending> {
        self.pending.iter_mut().find(|p| p.id() == id)
    }

    /// A send's (or a retry's) outcome.
    pub fn settle(&mut self, id: &str, outcome: &Outcome) -> Settled {
        let Some(pending) = self.get_mut(id) else {
            return Settled::Unknown;
        };
        match outcome {
            Outcome::Done(_) => {
                self.pending.retain(|p| p.id() != id);
                Settled::Accepted
            }
            Outcome::Refused { message, .. } | Outcome::NotSent(message) => {
                pending.state = State::Failed(message.clone());
                Settled::Kept
            }
            Outcome::Uncertain(message) => {
                pending.state = State::Uncertain(message.clone());
                Settled::Uncertain
            }
        }
    }

    /// The request that asks whether the daemon has this submission (`_sill/submissions.get`).
    pub fn check(&mut self, id: &str) -> Option<SubmissionKey> {
        let pending = self.get_mut(id)?;
        if !matches!(pending.state, State::Uncertain(_)) {
            return None;
        }
        pending.state = State::Checking;
        Some(SubmissionKey { session_id: pending.submit.session_id.clone(), submission_id: pending.submit.submission_id.clone() })
    }

    /// The check's outcome: the record means it arrived; `not_found` means it never did.
    pub fn checked(&mut self, id: &str, outcome: &Outcome) -> Settled {
        let Some(pending) = self.get_mut(id) else {
            return Settled::Unknown;
        };
        match outcome {
            Outcome::Done(value) if serde_json::from_value::<Submission>(value.clone()).is_ok() => {
                self.pending.retain(|p| p.id() != id);
                Settled::Accepted
            }
            Outcome::Refused { code, .. } if code.as_deref() == Some("not_found") => {
                pending.state = State::Failed("The daemon never received it: nothing was sent.".into());
                Settled::Kept
            }
            other => {
                pending.state = State::Uncertain(other.problem().unwrap_or("the daemon's answer was unexpected").to_string());
                Settled::Uncertain
            }
        }
    }

    /// The same request again, with the same identity; only for a failed or uncertain prompt.
    pub fn retry(&mut self, id: &str) -> Option<Submit> {
        let pending = self.get_mut(id)?;
        if !matches!(pending.state, State::Failed(_) | State::Uncertain(_)) {
            return None;
        }
        pending.state = State::Sending;
        Some(pending.submit.clone())
    }

    /// Take a prompt back out (to edit it, or to drop it).
    pub fn take(&mut self, id: &str) -> Option<Pending> {
        let index = self.pending.iter().position(|p| p.id() == id)?;
        Some(self.pending.remove(index))
    }

    /// Prompts the conversation now shows as the daemon's are done with, whatever their state.
    pub fn reconcile(&mut self, conversation: &Conversation) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| !conversation.submissions.contains_key(p.id()));
        self.pending.len() != before
    }

    /// Uncertain prompts, for checking once the session is current again.
    pub fn uncertain(&self) -> Vec<String> {
        self.pending.iter().filter(|p| matches!(p.state, State::Uncertain(_))).map(|p| p.id().to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attachment_retry_preserves_identity_and_bytes() {
        let mut outbox = Outbox::default();
        let original = outbox.send_with_attachments(
            "s",
            String::new(),
            vec![sill_proto::sfp::Attachment::Inline { name: "screenshot.png".into(), mime: "image/png".into(), data_base64: "YWJj".into() }],
        );
        assert_eq!(outbox.settle(&original.submission_id, &Outcome::Uncertain("lost".into())), Settled::Uncertain);
        assert_eq!(outbox.retry(&original.submission_id).unwrap(), original);
    }

    #[test]
    fn a_refused_prompt_keeps_its_text_and_a_lost_one_is_checked_not_resent() {
        let mut outbox = Outbox::default();
        let first = outbox.send("s", "hello".into());
        let refused = Outcome::Refused { code: Some("invalid_state".into()), message: "the session has ended".into() };
        assert_eq!(outbox.settle(&first.submission_id, &refused), Settled::Kept);
        let kept = outbox.get(&first.submission_id).unwrap();
        assert_eq!(kept.text(), "hello");
        assert!(matches!(&kept.state, State::Failed(m) if m.contains("ended")));
        let again = outbox.retry(&first.submission_id).unwrap();
        assert_eq!(again.submission_id, first.submission_id, "a retry keeps its identity");

        let second = outbox.send("s", "world".into());
        let lost = Outcome::Uncertain("the daemon closed the connection".into());
        assert_eq!(outbox.settle(&second.submission_id, &lost), Settled::Uncertain);
        assert_eq!(outbox.uncertain(), std::slice::from_ref(&second.submission_id));
        let key = outbox.check(&second.submission_id).unwrap();
        assert_eq!(key.submission_id, second.submission_id);
        assert!(outbox.check(&second.submission_id).is_none(), "one check at a time");
        let never = Outcome::Refused { code: Some("not_found".into()), message: "not found".into() };
        assert_eq!(outbox.checked(&second.submission_id, &never), Settled::Kept);
        assert!(matches!(outbox.get(&second.submission_id).unwrap().state, State::Failed(_)));

        let third = outbox.send("s", "arrived".into());
        outbox.settle(&third.submission_id, &lost);
        outbox.check(&third.submission_id);
        let record = json!({"submission_id": third.submission_id, "session_id":"s",
            "accepted_seq":"9", "agent_generation_id":"g", "text":"arrived", "attachments":[],
            "acceptance":"accepted", "delivery":"sent", "held":false, "insertion":"inserted",
            "outcome":"pending"});
        assert_eq!(outbox.checked(&third.submission_id, &Outcome::Done(record)), Settled::Accepted);
        assert!(outbox.get(&third.submission_id).is_none());
    }
}
