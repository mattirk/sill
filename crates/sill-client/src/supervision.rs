//! What needs the person in each open session — a question waiting, a failed turn, a prompt that
//! did not arrive, a lost connection — so a frontend can keep it visible wherever the person is
//! looking (docs/sill-gui.md, implementation direction 4).
use crate::Conversation;
use crate::lane::Status;
use crate::outbox::{Outbox, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Working,
    /// Waiting on the person (a question).
    Waiting,
    /// Not known: never shown as idle (docs/protocol.md, session activity).
    Unknown,
}

/// How urgently a session wants the person, least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Calm,
    Working,
    /// Something went wrong: a failed turn, an undelivered prompt, a lost connection.
    Problem,
    /// The agent is blocked on the person.
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub activity: Activity,
    /// Open questions: identity and title.
    pub questions: Vec<(String, String)>,
    /// The last turn failed: why.
    pub failed: Option<String>,
    /// Prompts that did not arrive or may not have.
    pub undelivered: usize,
    /// The connection's trouble.
    pub link: Option<String>,
    /// Working time of the current turn in milliseconds, at `now_ms`.
    pub working_ms: Option<u64>,
    /// The time the agent has worked in this session, at `now_ms`, whether or not it works now
    /// (the timer, shown when the status is hovered).
    pub worked_ms: Option<u64>,
    /// The work running is sill's summary turn (`SessionState::summarizing`), not the agent's
    /// own: shown in its own colour.
    pub summarizing: bool,
}

impl Attention {
    pub fn of(conversation: &Conversation, outbox: &Outbox, status: &Status, now_ms: u64) -> Self {
        let session = conversation.session.as_ref();
        let activity = match session.map(|s| s.activity.as_str()) {
            Some("idle") => Activity::Idle,
            Some("working") => Activity::Working,
            Some("waiting") => Activity::Waiting,
            _ => Activity::Unknown,
        };
        // a failure is news until another turn starts
        let running = session.is_some_and(|s| s.turn_id.is_some());
        let failed = conversation
            .last_turn_id()
            .filter(|_| !running)
            .and_then(|turn| conversation.turns.get(turn))
            .filter(|turn| turn.outcome == "failed")
            .map(|turn| turn.reason.as_ref().map_or_else(|| "the turn failed".into(), |r| r.message.clone()));
        let working_ms = session.and_then(|s| {
            let since = s.active_since_ms?;
            Some(s.active_ms + now_ms.saturating_sub(since))
        });
        let worked_ms = session.map(|s| s.active_ms + s.active_since_ms.map_or(0, |since| now_ms.saturating_sub(since)));
        Self {
            activity,
            questions: conversation.open_questions().map(|q| (q.question_id.clone(), q.title.clone())).collect(),
            failed,
            undelivered: outbox.pending().iter().filter(|p| matches!(p.state, State::Failed(_) | State::Uncertain(_))).count(),
            link: status.problem().map(str::to_owned),
            working_ms,
            worked_ms,
            summarizing: session.is_some_and(|s| s.summarizing.is_some()),
        }
    }

    pub fn level(&self) -> Level {
        if !self.questions.is_empty() {
            Level::Question
        } else if self.failed.is_some() || self.undelivered > 0 || self.link.is_some() {
            Level::Problem
        } else if matches!(self.activity, Activity::Working | Activity::Waiting) {
            Level::Working
        } else {
            Level::Calm
        }
    }

    /// One short line for a status strip.
    pub fn summary(&self) -> String {
        if let Some((_, title)) = self.questions.first() {
            return match self.questions.len() {
                1 => format!("Waiting for your answer: {title}"),
                n => format!("{n} questions waiting: {title}, …"),
            };
        }
        if let Some(link) = &self.link {
            return format!("Reconnecting: {link}");
        }
        if self.undelivered > 0 {
            return "A prompt did not reach the agent".into();
        }
        if let Some(failed) = &self.failed {
            return format!("The last turn failed: {failed}");
        }
        match self.activity {
            Activity::Working if self.summarizing => "Summarizing".into(),
            Activity::Working => "Working".into(),
            Activity::Waiting => "Waiting".into(),
            Activity::Idle => "Idle".into(),
            Activity::Unknown => "Activity unknown".into(),
        }
    }
}

/// m:ss below an hour, then h:mm:ss. Hours are never wrapped or truncated.
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 { format!("{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60) } else { format!("{}:{:02}", seconds / 60, seconds % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sill_proto::sfp::*;

    #[test]
    fn clock_preserves_minutes_and_grows_into_hours() {
        for (seconds, expected) in [
            (0, "0:00"),
            (599, "9:59"),
            (600, "10:00"),
            (1199, "19:59"),
            (1200, "20:00"),
            (3599, "59:59"),
            (3600, "1:00:00"),
            (35999, "9:59:59"),
            (36000, "10:00:00"),
            (360000, "100:00:00"),
        ] {
            assert_eq!(clock(seconds * 1000), expected);
        }
    }

    #[test]
    fn a_question_outranks_a_failure_which_outranks_work() {
        let mut view = Conversation::new("s", "sub", 1);
        let session: SessionState = serde_json::from_value(json!({
            "session_id":"s", "agent_generation_id":"g", "lifecycle":"running", "deleted":false,
            "activity":"working", "turn_id":"t2", "question_ids":[], "capabilities":{},
            "active_ms": 1000, "active_since_ms": 10_000
        }))
        .unwrap();
        view.session = Some(session);
        let attention = Attention::of(&view, &Outbox::default(), &Status::Live, 12_500);
        assert_eq!(attention.level(), Level::Working);
        assert_eq!(attention.working_ms.map(clock).as_deref(), Some("0:03"));
        assert!(!attention.summarizing);
        assert_eq!(attention.summary(), "Working");
        // sill's summary turn is work too, told apart
        view.session.as_mut().unwrap().summarizing = Some("checkpoint".into());
        let attention = Attention::of(&view, &Outbox::default(), &Status::Live, 12_500);
        assert_eq!((attention.level(), attention.summarizing), (Level::Working, true));
        assert_eq!(attention.summary(), "Summarizing");
        view.session.as_mut().unwrap().summarizing = None;

        let mut failed = Item {
            item_id: "i".into(),
            created_seq: "1".into(),
            turn_id: Some("t1".into()),
            submission_id: None,
            agent_generation_id: "g".into(),
            kind: "agent".into(),
            content: vec![],
            tool: None,
        };
        view.insert_item(failed.clone());
        view.turns.insert(
            "t1".into(),
            serde_json::from_value(json!({"turn_id":"t1", "agent_generation_id":"g",
                "activity":"idle", "outcome":"failed",
                "reason":{"code":"agent_error", "message":"the agent crashed"}}))
            .unwrap(),
        );
        // while the next turn runs, the earlier failure is not news
        assert!(Attention::of(&view, &Outbox::default(), &Status::Live, 0).failed.is_none());
        let idle = view.session.as_mut().unwrap();
        idle.activity = "idle".into();
        idle.turn_id = None;
        idle.active_since_ms = None;
        let attention = Attention::of(&view, &Outbox::default(), &Status::Live, 12_500);
        assert_eq!(attention.level(), Level::Problem);
        assert_eq!(attention.failed.as_deref(), Some("the agent crashed"));
        // a later turn that completed is the last one now
        failed.item_id = "j".into();
        failed.created_seq = "2".into();
        failed.turn_id = Some("t2".into());
        view.insert_item(failed);
        assert!(Attention::of(&view, &Outbox::default(), &Status::Live, 0).failed.is_none());

        view.questions.insert(
            "q".into(),
            serde_json::from_value(json!({"question_id":"q", "agent_generation_id":"g",
                "kind":"permission", "title":"Edit main.rs", "options":[], "fields":[],
                "state":"open"}))
            .unwrap(),
        );
        let attention = Attention::of(&view, &Outbox::default(), &Status::Live, 0);
        assert_eq!(attention.level(), Level::Question);
        assert_eq!(attention.summary(), "Waiting for your answer: Edit main.rs");
    }
}
