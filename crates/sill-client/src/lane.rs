//! A session lane: one connection of its own, subscribed to one session's conversation and kept
//! subscribed across connection loss and daemon restarts. It forwards the snapshot and then the
//! sequenced events in order; the frontend owns the [`Conversation`] and applies them.
//!
//! After a loss the lane resumes from the last event it forwarded (`attach` with `since`): the
//! daemon replays what was missed, or sends a replacement snapshot when the cursor is too old. A gap
//! or a resync request starts over from a fresh snapshot.
//!
//! One thread per lane, blocked on its inputs — no polling. The connection's notifications reach
//! it through a forwarder that ends with the connection. Dropping the [`Lane`] stops the thread and
//! closes the connection: the daemon ends the subscription, never the session or its agent.
//!
//! A connection per lane, not one shared by every open session: a busy session's stream cannot
//! hold up another's, a reconnect is per session, and the daemon's limit of eight snapshot leases
//! per connection never applies across sessions.
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use sill_proto::Client;
use sill_proto::sessions::{Session, SessionKey};
use sill_proto::sfp::{Attach, Attached, Cursor, Detach, SessionEvent};

use crate::{Connector, Conversation, refusal_code};

/// How a lane may attach. The daemon's `attach` starts a session's jail and agent when they are
/// not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The person opened this session: attaching may start it.
    Open,
    /// Supervision only: before every attach the lane checks the session is running, and leaves
    /// with [`Update::NotRunning`] when it is not, so watching never starts an agent.
    Watch,
}

#[derive(Debug, Clone)]
pub enum Update {
    /// Connecting; after the first attempt, the error that ended the previous one.
    Connecting { attempt: u32, error: Option<String> },
    /// A complete view, replacing whatever the frontend had for this session.
    Snapshot(Box<Conversation>),
    /// The subscription was restored without a snapshot: the frontend's view stays, and events
    /// from its cursor on follow under this subscription.
    Resubscribed { subscription_id: String },
    /// The next event, in sequence; duplicates and foreign subscriptions are already dropped.
    Event(Box<SessionEvent>),
    /// The view is complete and current; later events are live.
    Live,
    /// Watching stopped: the session is not running. Open it to start it. The lane has left.
    NotRunning,
    /// The session ended or no longer exists. The lane has left.
    Gone(String),
    /// The daemon sent what this frontend cannot read; following stopped. The lane has left.
    Stopped(String),
}

/// Where a session's lane stands, as its updates tell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    /// No lane: never opened in this window.
    #[default]
    Closed,
    Connecting,
    /// Connection lost or refused; retrying. The last error.
    Reconnecting(String),
    /// Subscribed, catching up on what was missed.
    Loading,
    Live,
    NotRunning,
    Gone(String),
    Stopped(String),
}

impl Status {
    pub fn follow(&mut self, update: &Update) {
        *self = match update {
            Update::Connecting { error: None, .. } => match self {
                // a reconnect without an error to show yet keeps saying why
                Status::Reconnecting(why) => Status::Reconnecting(why.clone()),
                _ => Status::Connecting,
            },
            Update::Connecting { error: Some(error), .. } => Status::Reconnecting(error.clone()),
            Update::Snapshot(_) | Update::Resubscribed { .. } => Status::Loading,
            Update::Event(_) => return,
            Update::Live => Status::Live,
            Update::NotRunning => Status::NotRunning,
            Update::Gone(why) => Status::Gone(why.clone()),
            Update::Stopped(why) => Status::Stopped(why.clone()),
        }
    }

    /// The lane has left: a new one is needed to follow the session again.
    pub fn ended(&self) -> bool {
        matches!(self, Status::Closed | Status::NotRunning | Status::Gone(_) | Status::Stopped(_))
    }

    /// The trouble to show, if any.
    pub fn problem(&self) -> Option<&str> {
        match self {
            Status::Reconnecting(why) | Status::Gone(why) | Status::Stopped(why) => Some(why),
            _ => None,
        }
    }
}

enum Input {
    Stop,
    Resync,
    Open,
    Notification { epoch: u64, method: String, params: Value },
    Lost { epoch: u64 },
}

/// The handle; the lane lives as long as it does.
pub struct Lane {
    inputs: Sender<Input>,
}

impl Lane {
    /// Follow `session_id`, reporting to `sink` from the lane's thread. `sink` returning false
    /// (the frontend went away) also ends the lane.
    pub fn open(session_id: impl Into<String>, mode: Mode, connect: Connector, sink: impl FnMut(Update) -> bool + Send + 'static) -> Self {
        let (inputs, receiver) = mpsc::channel();
        let worker = Worker { session_id: session_id.into(), mode, connect, inputs: receiver, forward: inputs.clone(), sink, epoch: 0, cursor: None };
        std::thread::Builder::new().name("sill-lane".into()).spawn(move || worker.run()).expect("start a session lane");
        Self { inputs }
    }

    /// The frontend could not apply an event (a gap it saw): start over from a fresh snapshot.
    pub fn resync(&self) {
        let _ = self.inputs.send(Input::Resync);
    }

    /// The person opened the session: from now on attaching may start it.
    pub fn promote(&self) {
        let _ = self.inputs.send(Input::Open);
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        let _ = self.inputs.send(Input::Stop);
    }
}

/// The first retry soon, then doubling to five seconds.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200u64.saturating_mul(1 << attempt.min(5))).min(Duration::from_secs(5))
}

/// Hand the connection's notifications to the lane, tagged with the connection they came on;
/// `Lost` when it closes. Takes the receiver out of the client, so the client — and with it the
/// connection — can go while this thread waits.
fn forward(client: &Client, epoch: u64, to: Sender<Input>) {
    let (_, closed) = mpsc::channel();
    let notifications = std::mem::replace(&mut *client.notifications.lock().unwrap(), closed);
    let _ = std::thread::Builder::new().name("sill-lane-events".into()).spawn(move || {
        while let Ok((method, params)) = notifications.recv() {
            if to.send(Input::Notification { epoch, method, params }).is_err() {
                return;
            }
        }
        let _ = to.send(Input::Lost { epoch });
    });
}

enum Next {
    Reconnect(String),
    Exit,
}

struct Worker<S> {
    session_id: String,
    mode: Mode,
    connect: Connector,
    inputs: Receiver<Input>,
    forward: Sender<Input>,
    sink: S,
    /// Which connection notifications belong to; older ones are ignored.
    epoch: u64,
    /// What the frontend has: its subscription and the last event forwarded.
    cursor: Option<(String, u64)>,
}

impl<S: FnMut(Update) -> bool> Worker<S> {
    fn run(mut self) {
        let mut attempt = 0;
        let mut error = None;
        loop {
            if !(self.sink)(Update::Connecting { attempt, error: error.take() }) {
                return;
            }
            match (self.connect)() {
                Err(e) => error = Some(format!("{e:#}")),
                Ok(client) => {
                    self.epoch += 1;
                    forward(&client, self.epoch, self.forward.clone());
                    match self.serve(&client, &mut attempt) {
                        Next::Exit => return,
                        Next::Reconnect(why) => error = Some(why),
                    }
                    // the client goes here, closing its connection
                }
            }
            if !self.pause(backoff(attempt)) {
                return;
            }
            attempt = attempt.saturating_add(1);
        }
    }

    /// Wait before reconnecting; a stop still stops.
    fn pause(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inputs.recv_timeout(left) {
                Ok(Input::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Input::Resync) => self.cursor = None,
                Ok(Input::Open) => self.mode = Mode::Open,
                // notifications and losses of a connection already given up
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return true,
            }
        }
    }

    fn detach(&mut self, client: &Client) {
        if let Some((subscription_id, _)) = self.cursor.take() {
            let _ = client.call::<Value>("_sill/sessions.detach", Detach { session_id: self.session_id.clone(), subscription_id });
        }
    }

    /// Whether watching may attach now; `Err` is the lane's end, already reported.
    fn may_attach(&mut self, client: &Client) -> Result<bool, Next> {
        if self.mode == Mode::Open {
            return Ok(true);
        }
        let key = SessionKey { session_id: self.session_id.clone() };
        match client.call::<Session>("_sill/sessions.get", key) {
            Ok(s) if s.ended_at.is_some() => {
                (self.sink)(Update::Gone("the session ended".into()));
                Err(Next::Exit)
            }
            Ok(s) if !s.running => {
                (self.sink)(Update::NotRunning);
                Err(Next::Exit)
            }
            Ok(_) => Ok(true),
            Err(e) if refusal_code(&e).as_deref() == Some("not_found") => {
                (self.sink)(Update::Gone("the session no longer exists".into()));
                Err(Next::Exit)
            }
            Err(e) => Err(Next::Reconnect(format!("{e:#}"))),
        }
    }

    /// Attach, then forward events until the connection is lost or the lane stops.
    fn serve(&mut self, client: &Client, attempt: &mut u32) -> Next {
        'attach: loop {
            if let Err(next) = self.may_attach(client) {
                return next;
            }
            let since = self.cursor.as_ref().map(|(_, seq)| Cursor { session_id: self.session_id.clone(), seq: seq.to_string() });
            let attach = Attach { session_id: self.session_id.clone(), since };
            // through which sequence a replay goes; none after a snapshot
            let through = match client.call::<Attached>("_sill/sessions.attach", attach) {
                Ok(Attached::Snapshot { subscription_id, snapshot }) => {
                    let view = match Conversation::read_snapshot(client, &self.session_id, &subscription_id, &snapshot) {
                        Ok(view) => view,
                        Err(e) => return Next::Reconnect(format!("{e:#}")),
                    };
                    self.cursor = Some((subscription_id, view.seq));
                    if !(self.sink)(Update::Snapshot(Box::new(view))) {
                        return Next::Exit;
                    }
                    None
                }
                Ok(Attached::Replay { subscription_id, after, through }) => {
                    let ours = self.cursor.as_ref().map(|(_, seq)| *seq);
                    if after.seq.parse::<u64>().ok() != ours {
                        // not a replay of what we have: start over
                        self.cursor = Some((subscription_id, 0));
                        self.detach(client);
                        continue 'attach;
                    }
                    self.cursor = Some((subscription_id.clone(), ours.unwrap_or_default()));
                    if !(self.sink)(Update::Resubscribed { subscription_id }) {
                        return Next::Exit;
                    }
                    Some(through.seq.parse::<u64>().unwrap_or_default())
                }
                Err(e) => match refusal_code(&e).as_deref() {
                    Some("not_found" | "gone") => {
                        (self.sink)(Update::Gone(format!("{e:#}")));
                        return Next::Exit;
                    }
                    // the daemon's history does not reach our cursor (a replaced store)
                    Some("invalid_cursor") => {
                        self.cursor = None;
                        continue 'attach;
                    }
                    _ => return Next::Reconnect(format!("{e:#}")),
                },
            };
            let caught_up = |cursor: &Option<(String, u64)>| through.is_none_or(|t| cursor.as_ref().is_some_and(|(_, seq)| *seq >= t));
            let mut live = false;
            if caught_up(&self.cursor) {
                live = true;
                *attempt = 0;
                if !(self.sink)(Update::Live) {
                    return Next::Exit;
                }
            }
            loop {
                let Ok(input) = self.inputs.recv() else {
                    return Next::Exit;
                };
                match input {
                    Input::Stop => return Next::Exit,
                    Input::Open => self.mode = Mode::Open,
                    Input::Resync => {
                        self.detach(client);
                        continue 'attach;
                    }
                    Input::Lost { epoch } if epoch == self.epoch => {
                        return Next::Reconnect("the connection to the daemon was lost".into());
                    }
                    Input::Lost { .. } => {}
                    Input::Notification { epoch, method, params } => {
                        if epoch != self.epoch || method != "_sill/session.event" {
                            continue;
                        }
                        let event: SessionEvent = match serde_json::from_value(params) {
                            Ok(event) => event,
                            Err(e) => {
                                (self.sink)(Update::Stopped(format!(
                                    "the daemon sent an event this frontend cannot read ({e}); \
                                     following stopped — update the frontend"
                                )));
                                return Next::Exit;
                            }
                        };
                        let Some((subscription, seq)) = &mut self.cursor else {
                            continue;
                        };
                        if event.session_id != self.session_id || &event.subscription_id != subscription {
                            continue;
                        }
                        match event.seq.parse::<u64>() {
                            Ok(n) if n <= *seq => continue,
                            Ok(n) if n == *seq + 1 => *seq = n,
                            // a gap, or nonsense: never show partial history
                            _ => {
                                self.detach(client);
                                continue 'attach;
                            }
                        }
                        if !(self.sink)(Update::Event(Box::new(event))) {
                            return Next::Exit;
                        }
                        if !live && caught_up(&self.cursor) {
                            live = true;
                            *attempt = 0;
                            if !(self.sink)(Update::Live) {
                                return Next::Exit;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::fake::{Peer, connector, pair};
    use serde_json::json;
    use std::sync::mpsc::Receiver;

    pub(crate) fn session_state(activity: &str) -> Value {
        json!({"session_id":"s", "agent_generation_id":"g", "lifecycle":"running",
               "deleted":false, "activity":activity, "question_ids":[], "capabilities":{}})
    }

    fn lease(seq: u64) -> Value {
        json!({"mode":"snapshot", "subscription_id":"sub-1",
               "snapshot":{"token":"lease", "at":{"session_id":"s","seq":seq.to_string()},
                           "expires_at":"9999999999999"}})
    }

    /// Answer an attach with a snapshot holding only the session record, at `seq`.
    fn snapshot(peer: &mut Peer, seq: u64) {
        let attach = peer.expect("_sill/sessions.attach");
        assert!(attach["params"]["since"].is_null(), "{attach}");
        peer.reply(&attach, lease(seq));
        let page = peer.expect("_sill/sessions.snapshot");
        peer.reply(
            &page,
            json!({"at":{"session_id":"s","seq":seq.to_string()},
                   "records":[{"type":"session","session":session_state("idle")}],
                   "next_page":null}),
        );
    }

    fn appended(subscription: &str, seq: u64, text: &str) -> Value {
        json!({"session_id":"s", "subscription_id":subscription, "seq":seq.to_string(),
               "event":{"type":"item.text_appended", "item_id":"i", "block_id":"b", "text":text}})
    }

    fn next(updates: &Receiver<Update>) -> Update {
        updates.recv_timeout(Duration::from_secs(5)).expect("an update")
    }

    /// The next update that is not `Connecting`.
    fn settled(updates: &Receiver<Update>) -> Update {
        loop {
            match next(updates) {
                Update::Connecting { .. } => {}
                other => return other,
            }
        }
    }

    fn open(clients: Vec<Client>, mode: Mode) -> (Lane, Receiver<Update>) {
        let (tx, rx) = mpsc::channel();
        let lane = Lane::open("s", mode, connector(clients), move |u| tx.send(u).is_ok());
        (lane, rx)
    }

    fn text(update: Update) -> String {
        match update {
            Update::Event(event) => match event.event {
                sill_proto::sfp::Event::ItemTextAppended { text, .. } => text,
                other => panic!("{other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn a_lane_forwards_its_snapshot_and_events_in_order_and_resumes_from_its_cursor() {
        let (first, mut peer) = pair();
        let (second, mut again) = pair();
        let (_lane, updates) = open(vec![first, second], Mode::Open);
        snapshot(&mut peer, 3);
        let Update::Snapshot(view) = settled(&updates) else { panic!() };
        assert_eq!((view.seq, view.subscription_id.as_str()), (3, "sub-1"));
        assert!(matches!(next(&updates), Update::Live));
        // a duplicate and a foreign subscription's event are dropped; the rest arrive in order
        for event in [appended("sub-1", 3, "old"), appended("other", 4, "foreign"), appended("sub-1", 4, "four"), appended("sub-1", 5, "five")] {
            peer.notify("_sill/session.event", event);
        }
        assert_eq!(text(next(&updates)), "four");
        assert_eq!(text(next(&updates)), "five");
        // the daemon restarts: the lane comes back and asks to replay from 5
        drop(peer);
        let Update::Connecting { error, .. } = next(&updates) else { panic!() };
        assert!(error.unwrap().contains("lost"));
        let attach = again.expect("_sill/sessions.attach");
        assert_eq!(attach["params"]["since"], json!({"session_id":"s", "seq":"5"}));
        again.reply(
            &attach,
            json!({"mode":"replay", "subscription_id":"sub-2",
                   "after":{"session_id":"s","seq":"5"}, "through":{"session_id":"s","seq":"6"}}),
        );
        let Update::Resubscribed { subscription_id } = settled(&updates) else { panic!() };
        assert_eq!(subscription_id, "sub-2");
        again.notify("_sill/session.event", appended("sub-1", 6, "stale subscription"));
        again.notify("_sill/session.event", appended("sub-2", 6, "six"));
        assert_eq!(text(next(&updates)), "six");
        assert!(matches!(next(&updates), Update::Live), "caught up through 6");
    }

    #[test]
    fn a_gap_starts_over_from_a_fresh_snapshot() {
        let (client, mut peer) = pair();
        let (_lane, updates) = open(vec![client], Mode::Open);
        snapshot(&mut peer, 3);
        assert!(matches!(settled(&updates), Update::Snapshot(_)));
        assert!(matches!(next(&updates), Update::Live));
        peer.notify("_sill/session.event", appended("sub-1", 5, "after a gap"));
        let detach = peer.expect("_sill/sessions.detach");
        assert_eq!(detach["params"]["subscription_id"], "sub-1");
        peer.reply(&detach, json!({}));
        snapshot(&mut peer, 5);
        let Update::Snapshot(view) = settled(&updates) else { panic!() };
        assert_eq!(view.seq, 5);
    }

    #[test]
    fn watching_never_attaches_a_session_that_is_not_running() {
        let (client, mut peer) = pair();
        let (_lane, updates) = open(vec![client], Mode::Watch);
        let get = peer.expect("_sill/sessions.get");
        peer.reply(
            &get,
            json!({"session_id":"s", "agent":"claude", "title":"t", "title_kind":"", "created_at":"1",
                   "last_used":"1", "ended_at":null, "policy":"default", "auto_allow":false,
                   "memory_scope":"", "summary":"", "summary_kind":"", "ghost":false,
                   "running":false, "pins":[], "settings":{}, "follows":null, "compacted":"0"}),
        );
        assert!(matches!(settled(&updates), Update::NotRunning));
        assert!(peer.hung_up(), "no attach: the lane left and closed its connection");
    }

    #[test]
    fn dropping_a_lane_closes_its_connection() {
        let (client, mut peer) = pair();
        let (lane, updates) = open(vec![client], Mode::Open);
        snapshot(&mut peer, 1);
        assert!(matches!(settled(&updates), Update::Snapshot(_)));
        drop(lane);
        assert!(peer.hung_up(), "the subscription's connection must close with the lane");
    }

    #[test]
    fn a_refused_attach_for_a_missing_session_ends_the_lane() {
        let (client, mut peer) = pair();
        let (_lane, updates) = open(vec![client], Mode::Open);
        let attach = peer.expect("_sill/sessions.attach");
        peer.refuse(&attach, "not_found", "session not found");
        assert!(matches!(settled(&updates), Update::Gone(_)));
    }
}
