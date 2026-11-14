use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::Task;
use serde_json::{Value, json};
use sill_client::actions::Actions;
use sill_client::conversation::item_text;
use sill_client::outbox::State;
use sill_proto::{Client, sfp::*};

use crate::{App, Message, SessionView, Tab, demo};

// --- harness ------------------------------------------------------------------------------------

/// The daemon's end of a socket pair: read the app's requests, answer or refuse them.
pub struct Daemon {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Daemon {
    /// The next request, whatever it is.
    pub fn next(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("a request")
    }

    /// The next request, which must be `method`. The window's background read of a session's
    /// permissions (`permissions.rs`: its preview first) may come in between: it is refused,
    /// which ends that read, unless the test expects it.
    pub fn expect(&mut self, method: &str) -> Value {
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("no request; expected {method}"));
            // reads that go out beside the one under test (the panel reads Host control's state
            // with its policy views, concurrently; a session's workspaces are read with its files
            // and the Changes page)
            let incidental = ["_sill/policy.preview", "_sill/host_control.state", "_sill/workspaces.session", "_sill/workspaces.list"];
            if incidental.contains(&request["method"].as_str().unwrap_or("")) && request["method"] != method {
                self.refuse(&request, "unavailable", "not in this test");
                continue;
            }
            assert_eq!(request["method"], method, "{request}");
            return request;
        }
    }

    pub fn reply(&mut self, request: &Value, result: impl serde::Serialize) {
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
        )
        .unwrap();
    }

    /// An `invalid_state` refusal with its reason, as the workspace methods refuse.
    pub fn refuse_for(&mut self, request: &Value, reason: &str, message: &str) {
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0", "id":request["id"],
                   "error":{"code":-32000, "message":message, "data":{"code":"invalid_state", "details":{"reason":reason, "paths":[]}}}})
        )
        .unwrap();
    }

    pub fn refuse(&mut self, request: &Value, code: &str, message: &str) {
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0", "id":request["id"],
                   "error":{"code":-32000, "message":message, "data":{"code":code}}})
        )
        .unwrap();
    }
}

pub fn daemon() -> (Client, Daemon) {
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let reader = BufReader::new(peer.try_clone().unwrap());
    (
        Client::over(socket).unwrap(),
        Daemon {
            reader,
            writer: peer,
        },
    )
}

/// A connector handing out these clients one connection at a time, then refusing.
pub fn connector(clients: Vec<Client>) -> sill_client::Connector {
    let clients = Arc::new(Mutex::new(
        clients.into_iter().map(Some).collect::<Vec<_>>(),
    ));
    Arc::new(move || {
        let mut all = clients.lock().unwrap();
        match all.iter_mut().find_map(Option::take) {
            Some(client) => Ok(client),
            None => anyhow::bail!("sill daemon at /fake: connection refused"),
        }
    })
}

/// The demo's session as a live client would have it: its requests go to `clients`.
pub(crate) fn live_app(clients: Vec<Client>) -> App {
    let (mut app, _) = App::new(true, None);
    app.demo = false;
    app.actions = Some(Actions::new(connector(clients)));
    app
}

/// Wait for `future` on this thread.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a task never finished"
        );
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

/// Run `task` to its end and apply the messages it produces (its widget operations are dropped).
pub(crate) fn settle_task(app: &mut App, task: Task<Message>) {
    use iced::futures::StreamExt;
    let Some(stream) = iced_runtime::task::into_stream(task) else {
        return;
    };
    let messages: Vec<Message> = block_on(
        stream
            .filter_map(|action| async move {
                match action {
                    iced_runtime::Action::Output(message) => Some(message),
                    _ => None,
                }
            })
            .collect(),
    );
    for message in messages {
        let next = app.update(message);
        settle_task(app, next);
    }
}

fn type_text(app: &mut App, text: &str) {
    use iced::widget::text_editor::{Action, Edit};
    let _ = app.update(Message::Prompt(Action::Edit(Edit::Paste(Arc::new(
        text.to_string(),
    )))));
}

fn demo_view(app: &App) -> &SessionView {
    &app.views["demo"]
}

pub(crate) fn event(view: &sill_client::Conversation, payload: Event) -> SessionEvent {
    SessionEvent {
        session_id: view.session_id.clone(),
        subscription_id: view.subscription_id.clone(),
        seq: (view.seq + 1).to_string(),
        event: payload,
    }
}

/// A second open session beside the demo's, with a conversation of its own.
fn add_session(app: &mut App, id: &str) {
    app.tabs.push(Tab {
        id: id.into(),
        title: format!("{id}-title"),
        agent: "codex".into(),
        running: true,
    });
    let mut view = SessionView::new(id);
    view.conversation = sill_client::Conversation::new(id, "sub", 0);
    view.conversation.session = demo::conversation().session.map(|mut s| {
        s.session_id = id.into();
        s.activity = "idle".into();
        s.turn_id = None;
        s
    });
    view.status = sill_client::lane::Status::Live;
    app.views.insert(id.into(), view);
}

pub(crate) fn simulator(app: &App, size: (f32, f32)) -> iced_test::Simulator<'_, Message> {
    iced_test::Simulator::with_size(crate::snapshots::settings(), size, app.view())
}

// --- the conversation model ---------------------------------------------------------------------

#[test]
fn duplicate_foreign_and_missing_events_do_not_corrupt_history() {
    let mut view = demo::conversation();
    let append = demo::append(4, "hello 🦀");
    assert!(view.accept(append.clone()).unwrap().is_some());
    assert!(view.accept(append).unwrap().is_none());
    let before = view.transcript();
    let mut foreign = demo::append(5, "wrong");
    foreign.subscription_id = "old-subscription".into();
    assert!(view.accept(foreign).unwrap().is_none());
    assert!(view.accept(demo::append(6, "gap")).is_err());
    assert_eq!(view.seq, 4);
    assert_eq!(view.transcript(), before);
    assert!(
        view.accept(demo::append(5, " contiguous"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn older_item_appends_stay_in_place_and_unknown_blocks_have_text_fallback() {
    let mut view = demo::conversation();
    let mut later = view.item("agent").unwrap().clone();
    later.item_id = "later".into();
    later.created_seq = "10".into();
    later.content = vec![Content {
        kind: "future_extension".into(),
        ..Content::text("fallback")
    }];
    view.accept(event(&view, Event::ItemCreated { item: later }))
        .unwrap();
    view.accept(demo::append(5, "arrived later")).unwrap();
    let ids: Vec<_> = view
        .visible_items()
        .into_iter()
        .map(|i| i.item_id.as_str())
        .collect();
    assert_eq!(ids, ["user", "tool", "agent", "later"]);
    assert!(item_text(view.item("agent").unwrap()).ends_with("arrived later"));
    assert_eq!(item_text(view.item("later").unwrap()), "fallback");
}

#[test]
fn queued_prompt_becomes_visible_on_dispatch_and_question_resolves() {
    let mut view = demo::conversation();
    view.item_mut("user").unwrap().submission_id = Some("queued".into());
    let mut submission: Submission = serde_json::from_value(json!({
        "submission_id":"queued", "session_id":"demo", "accepted_seq":"1",
        "agent_generation_id":"demo-generation", "text":"queued text", "attachments":[],
        "acceptance":"accepted", "delivery":"queued", "held":false,
        "insertion":"pending", "outcome":"pending"
    }))
    .unwrap();
    view.accept(event(
        &view,
        Event::SubmissionUpdated {
            submission: submission.clone(),
        },
    ))
    .unwrap();
    assert!(!view.visible_items().iter().any(|i| i.kind == "user"));
    submission.delivery = "sent".into();
    view.accept(event(&view, Event::SubmissionUpdated { submission }))
        .unwrap();
    assert!(view.visible_items().iter().any(|i| i.kind == "user"));
    for event in demo::waiting(view.seq + 1) {
        view.accept(event).unwrap();
    }
    assert!(view.open_questions().next().is_some());
    let mut question = view.questions["demo-question"].clone();
    question.state = "answered".into();
    view.accept(event(&view, Event::QuestionUpdated { question }))
        .unwrap();
    assert!(view.open_questions().next().is_none());
}

fn read_request(reader: &mut BufReader<UnixStream>, method: &str) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], method);
    request
}

fn reply(writer: &mut UnixStream, request: &Value, result: impl serde::Serialize) {
    writeln!(
        writer,
        "{}",
        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
    )
    .unwrap();
}

#[test]
fn fragmented_snapshot_and_live_handoff_over_real_protocol_transport() {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let source = demo::conversation();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        let request = read_request(&mut reader, "_sill/sessions.attach");
        assert_eq!(request["params"]["session_id"], "demo");
        assert!(request["params"]["since"].is_null());
        let cursor = Cursor {
            session_id: "demo".into(),
            seq: "3".into(),
        };
        reply(
            &mut peer,
            &request,
            Attached::Snapshot {
                subscription_id: "demo-subscription".into(),
                snapshot: SnapshotLease {
                    token: "lease".into(),
                    at: cursor.clone(),
                    expires_at: "9999999999999".into(),
                },
            },
        );
        // Both arrive while the immutable snapshot is still loading.
        for notification in [demo::append(3, "duplicate"), demo::append(4, " live 🦀")] {
            writeln!(
                peer,
                "{}",
                json!({"jsonrpc":"2.0","method":"_sill/session.event","params":notification})
            )
            .unwrap();
        }
        let request = read_request(&mut reader, "_sill/sessions.snapshot");
        assert_eq!(request["params"]["token"], "lease");
        reply(
            &mut peer,
            &request,
            SnapshotPage {
                at: cursor.clone(),
                records: vec![Record::Session {
                    session: source.session.clone().unwrap(),
                }],
                next_page: Some("first-fragment".into()),
                fragment: None,
                expires_at: None,
            },
        );
        let record = serde_json::to_string(&Record::Item {
            item: source.item("agent").unwrap().clone(),
        })
        .unwrap();
        let cut = record
            .char_indices()
            .nth(record.chars().count() / 2)
            .unwrap()
            .0;
        for (page, offset, fragment, last) in [
            ("first-fragment", 0, &record[..cut], false),
            ("last-fragment", cut, &record[cut..], true),
        ] {
            let request = read_request(&mut reader, "_sill/sessions.snapshot");
            assert_eq!(request["params"]["page"], page);
            reply(
                &mut peer,
                &request,
                SnapshotPage {
                    at: cursor.clone(),
                    records: vec![],
                    next_page: (!last).then(|| "last-fragment".into()),
                    fragment: Some(RecordFragment {
                        record: "agent-record".into(),
                        offset: offset.to_string(),
                        json: fragment.into(),
                        last,
                    }),
                    expires_at: None,
                },
            );
        }
    });
    let client = Client::over(socket).unwrap();
    let mut view = sill_client::Conversation::attach(&client, "demo").unwrap();
    assert_eq!(view.seq, 3);
    for expected in [false, true] {
        let (_, params) = client
            .notifications
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            view.accept(serde_json::from_value(params).unwrap())
                .unwrap()
                .is_some(),
            expected
        );
    }
    assert!(item_text(view.item("agent").unwrap()).ends_with(" live 🦀"));
    assert!(!view.transcript().contains("duplicate"));
    server.join().unwrap();
}

/// A replaced lane's late updates never reach the view: they carry the old lane's epoch.
#[test]
fn updates_from_a_replaced_lane_are_ignored() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    let current = app.views["other"].lane_epoch;
    let mut stale = demo::conversation();
    stale.session_id = "other".into();
    let _ = app.update(Message::Lane(
        "other".into(),
        current + 7,
        sill_client::lane::Update::Snapshot(Box::new(stale)),
    ));
    let _ = app.update(Message::Lane(
        "other".into(),
        current + 7,
        sill_client::lane::Update::Gone("old lane".into()),
    ));
    assert!(app.views["other"].conversation.items().is_empty());
    assert_eq!(app.views["other"].status, sill_client::lane::Status::Live);
}

// --- the person's requests ----------------------------------------------------------------------

/// Each action is one public SFP request of the exact shape the daemon accepts (its request
/// structs refuse unknown fields): send, stop, answer, a mode, an option, unqueue.
#[test]
fn actions_send_the_protocol_requests() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    // an empty composer sends nothing
    let task = app.update(Message::Send);
    settle_task(&mut app, task);
    type_text(&mut app, "hello agent\nsecond line");
    let task = app.update(Message::Send);
    assert!(
        demo_view(&app).draft.text().trim().is_empty(),
        "the composer clears on send"
    );
    let request = daemon.expect("_sill/sessions.submit");
    let params = &request["params"];
    assert_eq!(params["session_id"], "demo");
    assert_eq!(params["text"], "hello agent\nsecond line");
    assert_eq!(params["attachments"], json!([]));
    assert!(
        params["submission_id"]
            .as_str()
            .unwrap()
            .starts_with("gui-prompt-")
    );
    serde_json::from_value::<Submit>(params.clone()).unwrap();
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).outbox.pending().is_empty(), "accepted");

    let task = app.update(Message::Stop);
    let request = daemon.expect("_sill/sessions.cancel");
    assert_eq!(
        request["params"],
        json!({"session_id":"demo", "turn_id":"demo-turn"})
    );
    daemon.reply(&request, json!({"requested": true}));
    settle_task(&mut app, task);

    let task = app.update(Message::Answer {
        question_id: "q1".into(),
        option_id: "allow".into(),
    });
    let again = app.update(Message::Answer {
        question_id: "q1".into(),
        option_id: "allow".into(),
    });
    let request = daemon.expect("_sill/questions.answer");
    assert_eq!(
        request["params"]["answer"],
        json!({"type":"permission", "option_id":"allow"})
    );
    assert!(
        request["params"]["answer_id"]
            .as_str()
            .unwrap()
            .starts_with("gui-answer-")
    );
    serde_json::from_value::<AnswerQuestion>(request["params"].clone()).unwrap();
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    settle_task(&mut app, again);

    let pick = |value: &str, boolean| crate::Pick {
        value: value.into(),
        title: value.into(),
        boolean,
    };
    let task = app.update(Message::SetMode(pick("plan", None)));
    let request = daemon.expect("_sill/sessions.control");
    assert_eq!(
        request["params"]["control"],
        json!({"type":"set_mode", "mode_id":"plan"}),
        "a second press on an answer in flight sent nothing"
    );
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    let task = app.update(Message::SetOption("web".into(), pick("true", Some(true))));
    let request = daemon.expect("_sill/sessions.control");
    assert_eq!(
        request["params"]["control"],
        json!({"type":"set_config", "option_id":"web", "value":{"type":"bool", "value":true}})
    );
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);

    let task = app.update(Message::Unqueue("s1".into()));
    let request = daemon.expect("_sill/submissions.cancel");
    assert_eq!(
        request["params"],
        json!({"session_id":"demo", "submission_id":"s1"})
    );
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).failed.is_none());
}

/// Send now asks the daemon to send a queued prompt next (it stops the turn itself), only of a
/// daemon that answered `sill.send_now`; an older one is told apart without a request.
#[test]
fn send_now_needs_the_capability() {
    let (older, _quiet) = daemon();
    let mut app = live_app(vec![older]);
    let task = app.update(Message::SendNow("s1".into()));
    settle_task(&mut app, task);
    let failed = demo_view(&app).failed.clone().expect("refused here");
    assert!(failed.contains("sill.send_now"), "{failed}");

    let (client, mut daemon) = daemon();
    let initializing = std::thread::spawn(move || {
        client.initialize().unwrap();
        client
    });
    let request = daemon.expect("_sill/initialize");
    assert_eq!(
        request["params"]["capabilities"]["sill.send_now"], 1,
        "offered"
    );
    let answer = sill_proto::initialize::Initialized {
        protocol: sill_proto::initialize::PROTOCOL_ID.into(),
        daemon: sill_proto::initialize::Peer {
            name: "sill".into(),
            version: "test".into(),
        },
        capabilities: [("sill.send_now".to_string(), 1)].into(),
        limits: Default::default(),
    };
    daemon.reply(&request, serde_json::to_value(answer).unwrap());
    let client = initializing.join().unwrap();
    let mut app = live_app(vec![client]);
    let task = app.update(Message::SendNow("s1".into()));
    let request = daemon.expect("_sill/submissions.send_now");
    assert_eq!(
        request["params"],
        json!({"session_id":"demo", "submission_id":"s1"})
    );
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).failed.is_none());
}

/// Stop removes prompts still queued first, then stops the turn.
#[test]
fn stop_removes_queued_prompts_before_stopping() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let view = app.views.get_mut("demo").unwrap();
    let submission: Submission = serde_json::from_value(json!({
        "submission_id":"waiting", "session_id":"demo", "accepted_seq":"4",
        "agent_generation_id":"demo-generation", "text":"next", "attachments":[],
        "acceptance":"accepted", "delivery":"queued", "held":false,
        "insertion":"pending", "outcome":"pending"
    }))
    .unwrap();
    let next = event(&view.conversation, Event::SubmissionUpdated { submission });
    view.accept(next);
    let task = app.update(Message::Stop);
    let first = daemon.expect("_sill/submissions.cancel");
    assert_eq!(first["params"]["submission_id"], "waiting");
    daemon.reply(&first, json!({}));
    let second = daemon.expect("_sill/sessions.cancel");
    daemon.reply(&second, json!({"requested": true}));
    settle_task(&mut app, task);
}

/// Regression (review of 934097b): a refused Send lost the prompt — the composer cleared before
/// the daemon answered and only an error remained. Now the text comes back, a prompt that may
/// have arrived is checked rather than resent, and one typed meanwhile is never overwritten.
#[test]
fn a_refused_send_gives_the_prompt_back_and_an_uncertain_one_is_checked() {
    let (first, mut daemon) = daemon();
    let (third, mut checker) = self::daemon();
    let mut app = live_app(vec![first, third]);

    type_text(&mut app, "keep me");
    let task = app.update(Message::Send);
    let request = daemon.expect("_sill/sessions.submit");
    daemon.refuse(&request, "invalid_state", "the session has ended");
    settle_task(&mut app, task);
    let view = demo_view(&app);
    assert_eq!(view.draft.text().trim_end(), "keep me");
    assert!(
        view.failed
            .as_ref()
            .unwrap()
            .contains("the session has ended")
    );
    assert!(view.outbox.pending().is_empty());

    // typed again before the refusal came back: the new text stays, the old prompt waits
    let task = app.update(Message::Send);
    let request = daemon.expect("_sill/sessions.submit");
    assert_eq!(request["params"]["text"], "keep me");
    let id = request["params"]["submission_id"]
        .as_str()
        .unwrap()
        .to_string();
    type_text(&mut app, "newer");
    daemon.refuse(&request, "invalid_state", "the session has ended");
    settle_task(&mut app, task);
    let view = demo_view(&app);
    assert_eq!(view.draft.text().trim_end(), "newer");
    assert!(matches!(
        view.outbox.get(&id).unwrap().state,
        State::Failed(_)
    ));
    let _ = app.update(Message::EditSend(id));
    assert_eq!(demo_view(&app).draft.text().trim_end(), "newer\n\nkeep me");

    // the connection goes with the request in hand: uncertain, not resent
    let _ = app.update(Message::Prompt(
        iced::widget::text_editor::Action::SelectAll,
    ));
    let _ = app.update(Message::Prompt(iced::widget::text_editor::Action::Edit(
        iced::widget::text_editor::Edit::Delete,
    )));
    type_text(&mut app, "did it arrive?");
    let task = app.update(Message::Send);
    // Read on the existing connection before closing it. Dropping it before Send raced
    // the client's EOF detection and sometimes never reached the expected next socket.
    let request = daemon.expect("_sill/sessions.submit");
    drop(daemon);
    settle_task(&mut app, task);
    let id = request["params"]["submission_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(matches!(
        demo_view(&app).outbox.get(&id).unwrap().state,
        State::Uncertain(_)
    ));
    assert!(demo_view(&app).draft.text().trim().is_empty());
    // checking asks for it by identity; never received, it comes back to the composer
    let task = app.update(Message::CheckSend(id.clone()));
    let request = checker.expect("_sill/submissions.get");
    assert_eq!(request["params"]["submission_id"], id.as_str());
    checker.refuse(&request, "not_found", "submission not found");
    settle_task(&mut app, task);
    assert_eq!(demo_view(&app).draft.text().trim_end(), "did it arrive?");
    assert!(demo_view(&app).outbox.pending().is_empty());
}

/// Regression (review of 934097b): an unsent draft followed the person to another tab, where
/// Send would have given it to the wrong agent. Drafts belong to their session.
#[test]
fn a_draft_stays_with_its_session() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    type_text(&mut app, "for the demo agent only");
    let _ = app.update(Message::Choose("other".into()));
    assert_eq!(app.selected.as_deref(), Some("other"));
    assert!(app.views["other"].draft.text().trim().is_empty());
    let mut ui = simulator(&app, (720.0, 860.0));
    assert!(
        ui.find("for the demo agent only").is_err(),
        "not drawn in the other tab"
    );
    drop(ui);
    let _ = app.update(Message::Send);
    assert!(
        app.views["other"].outbox.pending().is_empty(),
        "nothing to send there"
    );
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(
        demo_view(&app).draft.text().trim_end(),
        "for the demo agent only"
    );
}

// --- supervision --------------------------------------------------------------------------------

/// A file takes the full view without a bottom status panel; its close returns to the conversation.
#[test]
fn a_file_has_no_bottom_status_panel_and_can_be_closed() {
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let _ = app.update(Message::Files(Action::ToggleDrawer));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("README.md").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(!demo_view(&app).conversation_shown());
    let mut ui = simulator(&app, (1000.0, 860.0));
    assert!(
        ui.find("Waiting for your answer: Edit conversation.rs")
            .is_err()
    );
    assert!(ui.find("‹ Conversation").is_err());
    drop(ui);
    // the file's own close is the way back
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click(iced::widget::Id::new("close-file")).unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).conversation_shown());
}

/// Another session waiting on the person stays marked in its tab while a file is open.
#[test]
fn other_sessions_waiting_on_the_person_are_marked() {
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    let view = app.views.get_mut("other").unwrap();
    for event in demo::waiting(1) {
        let mut event = event;
        event.session_id = "other".into();
        event.subscription_id = "sub".into();
        if let Event::SessionUpdated { session } = &mut event.event {
            session.session_id = "other".into();
        }
        assert!(view.accept(event), "applied");
    }
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.find("?").unwrap();
    drop(ui);
    let _ = app.update(Message::Files(Action::Open {
        path: sill_proto::sessions::HostPath::from_path(std::path::Path::new(
            "/home/demo/project/README.md",
        )),
        repo: None,
    }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("other-title").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::Choose(id)] if id == "other"),
        "{messages:?}"
    );
}

// --- rendering and input ------------------------------------------------------------------------

#[test]
fn bundled_faces_resolve_at_normal_and_bold_weights() {
    use iced::advanced::graphics::text::{cosmic_text::fontdb, font_system};
    crate::fonts::install();
    let mut system = font_system().write().unwrap();
    for family in [crate::fonts::SANS_FAMILY, crate::fonts::MONO_FAMILY] {
        for weight in [fontdb::Weight::NORMAL, fontdb::Weight::BOLD] {
            let query = fontdb::Query {
                families: &[fontdb::Family::Name(family)],
                weight,
                ..fontdb::Query::default()
            };
            let id = system.raw().db().query(&query).expect("a face");
            let face = system.raw().db().face(id).unwrap();
            assert!(face.families.iter().any(|(name, _)| name == family));
            assert_eq!(face.weight, weight, "{family} at {weight:?}");
        }
    }
}

/// Enter sends from the focused composer; Shift+Enter is the editor's own new line.
#[test]
fn enter_sends_and_shift_enter_is_a_new_line() {
    use iced::keyboard::{Key, Modifiers, key};
    use iced::widget::text_editor::{Binding, KeyPress, Status};
    let press = |modifiers| KeyPress {
        key: Key::Named(key::Named::Enter),
        modified_key: Key::Named(key::Named::Enter),
        physical_key: key::Physical::Code(key::Code::Enter),
        modifiers,
        text: Some("\n".into()),
        status: Status::Focused { is_hovered: false },
    };
    assert!(matches!(
        crate::ui::enter_sends(press(Modifiers::empty())),
        Some(Binding::Custom(Message::Send))
    ));
    assert!(matches!(
        crate::ui::enter_sends(press(Modifiers::SHIFT)),
        Some(Binding::Enter)
    ));
}

/// The question card's buttons answer it: a click on "Yes" in the rendered view is the answer.
#[test]
fn clicking_an_option_answers_the_question() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("Yes").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages.iter().any(|m| matches!(m,
            Message::Answer { question_id, option_id } if question_id == "demo-question" && option_id == "allow")),
        "{messages:?}"
    );
}

/// A stopped turn ends with a note; a completed one does not.
#[test]
fn a_stopped_turn_says_so() {
    let mut view = demo::conversation();
    let turn: Turn = serde_json::from_value(json!({
        "turn_id":"demo-turn", "agent_generation_id":"demo-generation", "activity":"idle",
        "outcome":"cancelled", "reason":{"code":"cancelled", "message":"stopped by the person"}
    }))
    .unwrap();
    view.accept(event(&view, Event::TurnUpdated { turn: turn.clone() }))
        .unwrap();
    assert_eq!(view.turn_end("demo-turn").as_deref(), Some("stopped"));
    let mut done = turn;
    done.outcome = "completed".into();
    view.accept(event(&view, Event::TurnUpdated { turn: done }))
        .unwrap();
    assert_eq!(view.turn_end("demo-turn"), None);
}

/// With no title bar, the top bar's empty space moves the window; a tab keeps its own click.
#[test]
fn the_top_bar_moves_the_window_and_tabs_keep_their_clicks() {
    let (mut app, _) = App::new(true, None);
    app.layout.mode = crate::layout::Mode::Drawer;
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.point_at(iced::Point::new(450.0, 18.0));
    let _ = ui.simulate(iced_test::simulator::click());
    ui.click("witty-willow").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(messages.first(), Some(Message::DragWindow)),
        "{messages:?}"
    );
    assert!(
        matches!(messages.get(1), Some(Message::Choose(id)) if id == "demo"),
        "{messages:?}"
    );
    assert_eq!(
        messages.len(),
        2,
        "a tab click must not also move the window: {messages:?}"
    );
}

/// Shelf mode is locked to its edge: the top bar does not move it and only the right edge,
/// corners included, resizes it. A free window has every edge, and corners resize both ways.
#[test]
fn shelf_mode_is_locked_to_its_edge_and_free_windows_have_corners() {
    use iced::window::Direction;
    let press = |app: &App, x: f32, y: f32| -> Vec<Message> {
        let mut ui = simulator(app, (720.0, 860.0));
        ui.point_at(iced::Point::new(x, y));
        let _ = ui.simulate(iced_test::simulator::click());
        ui.into_messages().collect()
    };
    let resized = |messages: &[Message]| {
        messages.iter().find_map(|m| match m {
            Message::ResizeWindow(direction) => Some(*direction),
            _ => None,
        })
    };
    let (mut app, _) = App::new(true, None);
    assert_eq!(app.layout.mode, crate::layout::Mode::Shelf);
    let bar = press(&app, 450.0, 18.0);
    assert!(!bar.iter().any(|m| matches!(m, Message::DragWindow)), "{bar:?}");
    assert!(resized(&press(&app, 2.0, 400.0)).is_none(), "no left edge");
    assert!(resized(&press(&app, 400.0, 2.0)).is_none(), "no top edge");
    assert!(resized(&press(&app, 400.0, 858.0)).is_none(), "no bottom edge");
    for y in [2.0, 400.0, 858.0] {
        assert!(
            matches!(resized(&press(&app, 718.0, y)), Some(Direction::East)),
            "the right edge, its corners included, changes the width only ({y})"
        );
    }
    app.layout.mode = crate::layout::Mode::Drawer;
    let at = |x, y| resized(&press(&app, x, y));
    assert!(matches!(at(2.0, 400.0), Some(Direction::West)));
    assert!(matches!(at(400.0, 2.0), Some(Direction::North)));
    assert!(matches!(at(2.0, 2.0), Some(Direction::NorthWest)));
    assert!(matches!(at(718.0, 2.0), Some(Direction::NorthEast)));
    assert!(matches!(at(2.0, 858.0), Some(Direction::SouthWest)));
    assert!(matches!(at(718.0, 858.0), Some(Direction::SouthEast)));
    assert!(matches!(at(718.0, 400.0), Some(Direction::East)));
}

/// Ctrl+Q quits; Q alone does not.
#[test]
fn ctrl_q_quits() {
    use iced::keyboard::{Event, Key, Location, Modifiers, key};
    let press = |modifiers| Event::KeyPressed {
        key: Key::Character("q".into()),
        modified_key: Key::Character("q".into()),
        physical_key: key::Physical::Code(key::Code::KeyQ),
        location: Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    };
    assert!(matches!(
        crate::keys::shortcut(press(Modifiers::COMMAND)),
        Some(Message::Quit)
    ));
    assert!(crate::keys::shortcut(press(Modifiers::empty())).is_none());
}

// --- the terminal -------------------------------------------------------------------------------

/// Wait until the pane's screen shows `text` (its reader thread fills it from the stream).
pub fn settle(pane: &crate::terminal::Pane, text: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !pane.screen().contains(text) {
        assert!(
            std::time::Instant::now() < deadline,
            "{text:?} never appeared:\n{}",
            pane.screen()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A connection over `socket`, its controls recorded in `seen`.
fn connection(
    socket: UnixStream,
    seen: Arc<Mutex<Vec<String>>>,
) -> sill_client::terminal::Connection {
    use sill_client::terminal::{Connection, Control};
    let close = socket.try_clone().unwrap();
    Connection {
        discarded: true,
        size: (40, 10),
        stream: sill_proto::transport::RawStream {
            reader: Box::new(socket.try_clone().unwrap()),
            writer: Box::new(socket),
            interrupt: Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
            shutdown_write: Arc::new(|| {}),
        },
        control: Box::new(move |c| {
            seen.lock().unwrap().push(match c {
                Control::Resize(cols, rows) => format!("resize {cols}x{rows}"),
                Control::Close => "close".into(),
            });
            Ok(())
        }),
    }
}

/// The terminal is the daemon's byte stream through the emulator: output reaches the screen with
/// its colours, keys and the emulator's own answers go back, a resize is a request, and the end
/// of the stream is the end of the shell.
#[test]
fn a_terminal_shows_its_stream_and_sends_keys_back() {
    use crate::terminal::{Event, Pane, Status};
    use std::io::Read;
    let (ours, mut remote) = UnixStream::pair().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let controls = Arc::new(Mutex::new(Vec::new()));
    let seen = controls.clone();
    let (mut pane, events) = Pane::open(40, 10, move |cols, rows| {
        assert_eq!((cols, rows), (40, 10));
        Ok(connection(ours, seen))
    });
    let next = || events.recv_blocking().unwrap();
    pane.accept(next());
    assert_eq!(pane.status, Status::Running);
    remote.write_all(b"hi \x1b[31mred\x1b[0m\r\n$ ").unwrap();
    settle(&pane, "hi red");
    let screen = pane.screen();
    assert!(
        screen.starts_with("[earlier output was not kept]\nhi red\n$"),
        "{screen}"
    );
    assert_eq!(pane.fg_at(1, 3), crate::style::P.error);
    assert_eq!(pane.fg_at(1, 0), crate::style::P.fg);

    pane.input(b"ls\r".to_vec());
    let mut typed = [0u8; 3];
    remote.read_exact(&mut typed).unwrap();
    assert_eq!(&typed, b"ls\r");
    // a program asking what the terminal is gets the emulator's answer
    remote.write_all(b"\x1b[c").unwrap();
    let mut answer = [0u8; 3];
    remote.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"\x1b[?");
    let mut rest = [0u8; 64];
    let _ = remote.read(&mut rest).unwrap();

    pane.resize(100, 30);
    pane.resize(100, 30);
    pane.close();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while controls.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*controls.lock().unwrap(), ["resize 100x30", "close"]);
    drop(remote);
    loop {
        let event = next();
        let ended = matches!(event, Event::Ended);
        pane.accept(event);
        if ended {
            break;
        }
    }
    assert_eq!(pane.status, Status::Ended);
}

/// Regression (review of 934097b): dropping or replacing a pane left its stream attached — the
/// emulator's answer channel kept the writer alive while the reader waited. The daemon allows
/// eight viewers per terminal, so repeated switching would lock the person out of their shell.
#[test]
fn dropping_a_pane_detaches_its_stream_even_after_the_emulator_answered() {
    use crate::terminal::{Pane, Status};
    use std::io::Read;
    let (ours, mut remote) = UnixStream::pair().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (mut pane, events) = Pane::open(40, 10, move |_, _| Ok(connection(ours, Arc::default())));
    pane.accept(events.recv_blocking().unwrap());
    assert_eq!(pane.status, Status::Running);
    // the emulator answers a device-attribute query: the path whose sender kept the link alive
    remote.write_all(b"\x1b[c").unwrap();
    let mut answer = [0u8; 3];
    remote.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"\x1b[?");
    let mut rest = [0u8; 64];
    let _ = remote.read(&mut rest).unwrap();
    drop(pane);
    let mut more = [0u8; 16];
    assert_eq!(
        remote.read(&mut more).unwrap(),
        0,
        "the stream must close when its pane goes"
    );
}

/// Keys as xterm sends them, which shells and full-screen programs expect.
#[test]
fn terminal_keys_are_encoded_as_xterm_sends_them() {
    use crate::terminal::encode;
    use alacritty_terminal::term::TermMode;
    use iced::keyboard::{Key, Modifiers, key::Named};
    let plain = TermMode::empty();
    let key = |k: Key, m: Modifiers, text: Option<&str>, mode| encode(&k, m, text, mode);
    let none = Modifiers::empty();
    let named = |n| Key::Named(n);
    let ch = |c: &str| Key::Character(c.into());
    assert_eq!(key(ch("a"), none, Some("a"), plain).unwrap(), b"a");
    assert_eq!(
        key(ch("ä"), none, Some("ä"), plain).unwrap(),
        "ä".as_bytes()
    );
    assert_eq!(key(ch("c"), Modifiers::CTRL, None, plain).unwrap(), [3]);
    assert_eq!(
        key(ch("x"), Modifiers::ALT, Some("x"), plain).unwrap(),
        b"\x1bx"
    );
    assert_eq!(
        key(named(Named::Enter), none, Some("\r"), plain).unwrap(),
        b"\r"
    );
    assert_eq!(
        key(named(Named::Backspace), none, None, plain).unwrap(),
        [0x7f]
    );
    assert_eq!(
        key(named(Named::Tab), Modifiers::SHIFT, None, plain).unwrap(),
        b"\x1b[Z"
    );
    assert_eq!(
        key(named(Named::ArrowUp), none, None, plain).unwrap(),
        b"\x1b[A"
    );
    assert_eq!(
        key(named(Named::ArrowUp), none, None, TermMode::APP_CURSOR).unwrap(),
        b"\x1bOA"
    );
    assert_eq!(
        key(named(Named::ArrowLeft), Modifiers::CTRL, None, plain).unwrap(),
        b"\x1b[1;5D"
    );
    assert_eq!(
        key(named(Named::Delete), none, None, plain).unwrap(),
        b"\x1b[3~"
    );
    assert_eq!(
        key(named(Named::F5), none, None, plain).unwrap(),
        b"\x1b[15~"
    );
    assert!(key(named(Named::Shift), Modifiers::SHIFT, None, plain).is_none());
}

/// The terminal button opens the shell in the prompt's place; typed keys go to it, not to the prompt.
#[test]
fn the_terminal_button_opens_the_shell_and_takes_the_keys() {
    let (mut app, _) = App::new(true, None);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("terminal-button")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(messages.as_slice(), [Message::ToggleTerminal]),
        "{messages:?}"
    );
    // the task carries the pane's events; dropping it would hang the shell up
    let _events = app.update(Message::ToggleTerminal);
    let pane = app
        .views
        .get_mut("demo")
        .unwrap()
        .terminal
        .as_mut()
        .unwrap();
    pane.accept(crate::terminal::Event::Opened);
    settle(pane, "project$");
    let mut ui = simulator(&app, (720.0, 860.0));
    let _ = ui.typewrite("ls");
    let typed: Vec<u8> = ui
        .into_messages()
        .filter_map(|m| match m {
            Message::TermAction(crate::terminal::Action::Input(bytes)) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(typed, b"ls");
    let _ = app.update(Message::ToggleTerminal);
    assert!(
        !demo_view(&app).terminal_shown && demo_view(&app).terminal.is_some(),
        "hiding only detaches the view"
    );
}

// --- files --------------------------------------------------------------------------------------

#[test]
fn file_drawer_buttons_open_the_viewer_and_return_to_conversation() {
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(
        crate::layout::Mode::Drawer,
    )));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("README.md").unwrap();
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Files(Action::Open { .. })))
    );
    for message in messages {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).files.reader.is_some());
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click(iced::widget::Id::new("close-file")).unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).files.reader.is_none());
}

#[test]
fn diff_numbers_distinguish_context_changes_and_untracked_markdown() {
    use crate::diff_ui::numbered_diff;
    assert_eq!(
        numbered_diff("@@ -3,2 +5,3 @@\n keep\n-old\n+new\n+extra"),
        vec![
            (None, None, "@@ -3,2 +5,3 @@"),
            (Some(3), Some(5), " keep"),
            (Some(4), None, "-old"),
            (None, Some(6), "+new"),
            (None, Some(7), "+extra")
        ]
    );
    assert!(
        numbered_diff("(untracked)\n- list item\n+ another item")
            .iter()
            .all(|(old, new, _)| old.is_none() && new.is_none())
    );
}

#[test]
fn narrow_drawer_opens_files_in_the_full_content_area() {
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::ToggleDrawer));
    let mut ui = simulator(&app, (480.0, 700.0));
    ui.click("README.md").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(!app.drawer.shown);
    assert!(demo_view(&app).files.reader.is_some());
}

/// In a window too narrow for both, the drawer opens over the conversation, which keeps its place
/// underneath.
#[test]
fn a_drawer_over_the_conversation_keeps_its_place() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((520.0, 400.0));
    live.apply(&mut app, Message::Latest);
    let mut before = None;
    live.frame(&app, |ui| before = scrolled(ui, "conversation"));
    app.width = 520.0;
    live.apply(
        &mut app,
        Message::Files(crate::files::Action::ToggleDrawer),
    );
    assert!(app.narrow(), "520 px does not fit the drawer beside");
    let mut after = None;
    live.frame(&app, |ui| after = scrolled(ui, "conversation"));
    let (before, after) = (before.unwrap(), after.unwrap());
    assert!(before > 100.0, "the conversation is longer than the window");
    // Opening the drawer keeps the conversation at its tail.
    assert!(
        after >= before - 1.0,
        "back towards the top: {before} → {after}"
    );
    assert!(app.current().unwrap().follow_tail);
}

/// The keys go between the modes: Ctrl+Shift+D from Shelf to Drawer and back, Ctrl+Shift+F into
/// Full and back to where it came from; putting Drawer mode's drawer away is Full; in
/// Shelf mode a chip or the drawer's toggle is a quick look over the conversation.
#[test]
fn the_keys_go_between_shelf_drawer_and_full() {
    use crate::layout::Mode;
    use crate::files::Action as Files;
    let (mut app, _) = App::new(true, None);
    assert_eq!(app.layout.mode, Mode::Shelf);
    let press = |c: &str| {
        crate::keys::shortcut(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Character(c.into()),
            modified_key: iced::keyboard::Key::Character(c.into()),
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT,
            text: None,
            repeat: false,
        })
    };
    assert!(press("E").is_none(), "Ctrl+Shift+E is free again");
    let _ = app.update(Message::Files(Files::ToggleDrawer));
    assert!(app.drawer.shown, "a quick look in Shelf mode");
    assert!(matches!(
        app.layout.drawer(true, 1200.0),
        crate::layout::Drawer::Over(_)
    ));
    // Shelf → Full → back to Shelf
    let _ = app.update(press("F").unwrap());
    assert_eq!(app.layout.mode, Mode::Full);
    let _ = app.update(press("F").unwrap());
    assert_eq!(app.layout.mode, Mode::Shelf);
    // Shelf → Drawer → Full → Escape back to Drawer
    let _ = app.update(press("D").unwrap());
    assert_eq!(app.layout.mode, Mode::Drawer);
    assert!(app.drawer.shown);
    let _ = app.update(press("F").unwrap());
    assert_eq!(app.layout.mode, Mode::Full);
    assert!(!app.drawer.shown);
    let _ = app.update(press("F").unwrap());
    assert_eq!(app.layout.mode, Mode::Drawer, "Ctrl+Shift+F leaves Full");
    // the drawer's own close in Drawer mode: the same window without it
    let _ = app.update(Message::Files(Files::ToggleDrawer));
    assert_eq!(app.layout.mode, Mode::Full);
    let _ = app.update(press("D").unwrap());
    assert_eq!(app.layout.mode, Mode::Shelf);
    assert!(!app.drawer.shown);
}

/// The `+` menu's foot switches the mode: Shelf, Drawer or Full.
#[test]
fn the_menu_switches_between_the_three_modes() {
    use crate::layout::Mode;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(crate::menu::Event::Toggle));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("view-full")).unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(app.menu.is_none(), "choosing a mode closes the menu");
    assert_eq!(app.layout.mode, Mode::Full);
    let _ = app.update(Message::Menu(crate::menu::Event::View(Mode::Drawer)));
    assert!(app.drawer.shown && app.layout.mode == Mode::Drawer);
    let _ = app.update(Message::Menu(crate::menu::Event::View(Mode::Shelf)));
    assert!(!app.drawer.shown && app.layout.mode == Mode::Shelf);
}

/// A long conversation builds only the rows near the view; the rest are gaps of their measured
/// height, so the conversation is as tall either way and a far row appears when scrolled to.
#[test]
fn dragging_the_drawer_keeps_a_long_conversation_s_far_rows_as_gaps() {
    use crate::layout::Action;
    use iced_test::selector::Candidate;
    let built = |ui: &mut Frame<'_, '_>, needle: &'static str| -> bool {
        ui.select(move |candidate| match candidate {
            Candidate::Text { content, .. } if content.contains(needle) => Some(()),
            Candidate::Custom { state, .. } => state
                .downcast_ref::<crate::selection::Selectable>()
                .filter(|t| t.text().contains(needle))
                .map(|_| ()),
            _ => None,
        })
        .is_some()
    };
    let mut app = crate::perf::long(400);
    // the drawer beside the conversation, in a window narrower than the measure: the drawer's
    // width decides the rows'
    app.layout.mode = crate::layout::Mode::Drawer;
    app.drawer.shown = true;
    app.width = 900.0;
    let mut live = Live::new((900.0, 600.0));
    live.frame(&app, |_| {});
    {
        let view = app.views.get_mut("demo").unwrap();
        view.viewport = 520.0;
        view.follow_tail = true;
    }
    let mut first = true;
    live.frame(&app, |ui| first = built(ui, "pattern-1' crates"));
    assert!(!first, "settled: the first rows are gaps");
    // the edge dragged: the far rows stay gaps at every move
    let _ = app.update(Message::Layout(Action::DragStart));
    for x in [690.0, 680.0, 670.0] {
        let _ = app.update(Message::Layout(Action::DragTo(x)));
        live.frame(&app, |ui| first = built(ui, "pattern-1' crates"));
        assert!(!first, "dragging to {x}: the first rows stay gaps");
    }
    // released: every row is measured once at the new width, then the far ones are gaps again
    let _ = app.update(Message::Layout(Action::DragEnd));
    live.frame(&app, |ui| first = built(ui, "pattern-1' crates"));
    assert!(first, "released: measured anew");
    live.frame(&app, |ui| first = built(ui, "pattern-1' crates"));
    assert!(!first, "then gaps again");
}

#[test]
fn a_long_conversation_builds_only_what_is_near_the_view() {
    use iced_test::selector::Candidate;
    let built = |ui: &mut Frame<'_, '_>, needle: &'static str| -> bool {
        ui.select(move |candidate| match candidate {
            Candidate::Text { content, .. } if content.contains(needle) => Some(()),
            Candidate::Custom { state, .. } => state
                .downcast_ref::<crate::selection::Selectable>()
                .filter(|t| t.text().contains(needle))
                .map(|_| ()),
            _ => None,
        })
        .is_some()
    };
    let height = |ui: &mut Frame<'_, '_>| -> Option<f32> {
        ui.select(|candidate| match candidate {
            Candidate::Scrollable {
                id: Some(found),
                content_bounds,
                ..
            } if *found == iced::widget::Id::new("conversation") => Some(content_bounds.height),
            _ => None,
        })
    };
    let mut app = crate::perf::long(400);
    let mut live = Live::new((720.0, 600.0));
    // the first frame measures every row; following the tail, the first rows are then gaps
    let mut full = None;
    live.frame(&app, |ui| full = height(ui));
    {
        let view = app.views.get_mut("demo").unwrap();
        view.viewport = 520.0;
        view.follow_tail = true;
    }
    let (mut tail, mut first, mut last) = (None, false, false);
    live.frame(&app, |ui| {
        tail = height(ui);
        first = built(ui, "pattern-1' crates");
        last = built(ui, "pattern-397' crates");
    });
    assert!(last && !first, "only the tail is built");
    let (full, tail) = (full.unwrap(), tail.unwrap());
    assert!(full > 20_000.0, "a long conversation: {full}");
    assert!(
        (full - tail).abs() < 1.0,
        "as tall with gaps: {full} vs {tail}"
    );
    // scrolled to the top: the first rows are built, the last are a gap
    {
        let view = app.views.get_mut("demo").unwrap();
        view.follow_tail = false;
        view.scroll_offset = 0.0;
    }
    let (mut top, mut first, mut last) = (None, false, false);
    live.frame(&app, |ui| {
        top = height(ui);
        first = built(ui, "pattern-1' crates");
        last = built(ui, "pattern-397' crates");
    });
    assert!(first && !last, "only the top is built");
    assert!((full - top.unwrap()).abs() < 1.0);
}

/// The herring opens the pages at the one last shown; the page tabs switch between them; Escape
/// goes back to the conversation. The Settings page shows the daemon's settings, and a theme
/// chosen there shows at once.
#[test]
fn the_herring_opens_changes_and_settings() {
    use crate::Page;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    assert!(app.changes.is_some() && app.settings_page.is_none());
    let task = app.update(Message::ShowPage(Page::Settings));
    settle_task(&mut app, task);
    assert!(app.changes.is_none());
    let page = app.settings_page.as_ref().unwrap();
    assert!(
        matches!(page.snapshot, Some(Ok(_))),
        "read at once in the demo"
    );
    for (category, labels) in [
        ("Interface", vec!["Keyboard shortcuts"]),
        ("Sessions", vec!["Timer autoshow", "Remember pins"]),
        ("Memory & history", vec!["Compact after"]),
    ] {
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.click(category).unwrap();
        let messages: Vec<_> = ui.into_messages().collect();
        for message in messages {
            let task = app.update(message);
            settle_task(&mut app, task);
        }
        let mut ui = simulator(&app, (720.0, 860.0));
        for label in labels {
            ui.find(label).unwrap();
        }
    }
    let flicked = app.herring_flick;
    assert!(flicked.is_some(), "the herring swims when the view changes");
    let _ = app.update(Message::Escape);
    assert!(
        app.settings_page.is_none(),
        "Escape: back to the conversation"
    );
    assert_ne!(app.herring_flick, flicked, "and again on the way back");
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    assert!(
        app.settings_page.is_some(),
        "the herring opens the page last shown"
    );
    // the theme shows before the daemon answers
    let before = crate::style::theme_id();
    let _ = app.update(Message::Settings(crate::settings_ui::Event::Change(
        sill_proto::admin::SettingChange::Theme {
            value: "dracula".into(),
        },
    )));
    assert_eq!(crate::style::theme_id(), "dracula");
    crate::style::set_theme(before);
}

// --- selection ----------------------------------------------------------------------------------

/// What a selection test drives: the Simulator (one view, built once) or a [`Live`] frame (the
/// view rebuilt between frames with the widget state kept, as the runtime does).
trait Drive {
    fn aim(&mut self, at: iced::Point);
    fn send(&mut self, events: Vec<iced::Event>);
    fn select<T, F>(&mut self, selector: F) -> Option<T>
    where
        T: Clone + Send + 'static,
        F: FnMut(iced_test::selector::Candidate<'_>) -> Option<T> + Send;
}

impl Drive for iced_test::Simulator<'_, Message> {
    fn aim(&mut self, at: iced::Point) {
        self.point_at(at);
    }

    fn send(&mut self, events: Vec<iced::Event>) {
        let _ = self.simulate(events);
    }

    fn select<T, F>(&mut self, selector: F) -> Option<T>
    where
        T: Clone + Send + 'static,
        F: FnMut(iced_test::selector::Candidate<'_>) -> Option<T> + Send,
    {
        self.find(selector).ok()
    }
}

/// The app's view rebuilt for every frame with the previous frame's widget state, the app's
/// tasks' widget operations (`snap_to_end`, …) run on the frame after, as the runtime does.
struct Live {
    renderer: iced::Renderer,
    cache: Option<iced_runtime::user_interface::Cache>,
    size: iced::Size,
    cursor: iced::mouse::Cursor,
    operations: Vec<Box<dyn iced::advanced::widget::Operation>>,
}

struct Frame<'a, 'b> {
    ui: &'b mut iced_runtime::UserInterface<'a, Message, iced::Theme, iced::Renderer>,
    renderer: &'b mut iced::Renderer,
    cursor: &'b mut iced::mouse::Cursor,
    messages: Vec<Message>,
}

impl Live {
    fn new(size: (f32, f32)) -> Self {
        use iced::advanced::renderer::Headless;
        let settings = crate::snapshots::settings();
        let renderer = block_on(<iced::Renderer as Headless>::new(
            settings.default_font,
            settings.default_text_size,
            None,
        ))
        .expect("a headless renderer");
        Self {
            renderer,
            cache: Some(iced_runtime::user_interface::Cache::default()),
            size: size.into(),
            cursor: iced::mouse::Cursor::Unavailable,
            operations: Vec::new(),
        }
    }

    /// One frame of `app`: queued operations, then `drive`, then drawing. Returns the messages
    /// the frame produced, not yet applied.
    fn frame(&mut self, app: &App, drive: impl FnOnce(&mut Frame<'_, '_>)) -> Vec<Message> {
        let mut ui = iced_runtime::UserInterface::build(
            app.view(),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        // as the runtime does: an operation may chain the next (focus traversal counts first)
        for operation in self.operations.drain(..) {
            let mut current = Some(operation);
            while let Some(mut operation) = current.take() {
                ui.operate(&self.renderer, operation.as_mut());
                if let iced::advanced::widget::operation::Outcome::Chain(next) = operation.finish()
                {
                    current = Some(next);
                }
            }
        }
        let mut frame = Frame {
            ui: &mut ui,
            renderer: &mut self.renderer,
            cursor: &mut self.cursor,
            messages: Vec::new(),
        };
        drive(&mut frame);
        let messages = frame.messages;
        ui.draw(
            &mut self.renderer,
            &crate::style::theme(),
            &iced::advanced::renderer::Style {
                text_color: crate::style::P.fg,
            },
            self.cursor,
        );
        self.cache = Some(ui.into_cache());
        messages
    }

    /// `message` applied to the app; what its task does to widgets happens on the next frame.
    fn apply(&mut self, app: &mut App, message: Message) {
        let task = app.update(message);
        self.run(app, task);
    }

    fn run(&mut self, app: &mut App, task: Task<Message>) {
        use iced::futures::StreamExt;
        let Some(stream) = iced_runtime::task::into_stream(task) else {
            return;
        };
        let actions: Vec<_> = block_on(stream.collect());
        for action in actions {
            match action {
                iced_runtime::Action::Widget(operation) => self.operations.push(operation),
                iced_runtime::Action::Output(message) => self.apply(app, message),
                _ => {}
            }
        }
    }
}

impl Drive for Frame<'_, '_> {
    fn aim(&mut self, at: iced::Point) {
        *self.cursor = iced::mouse::Cursor::Available(at);
    }

    fn send(&mut self, events: Vec<iced::Event>) {
        let _ = self.ui.update(
            &events,
            *self.cursor,
            self.renderer,
            &mut iced::advanced::clipboard::Null,
            &mut self.messages,
        );
    }

    fn select<T, F>(&mut self, selector: F) -> Option<T>
    where
        T: Clone + Send + 'static,
        F: FnMut(iced_test::selector::Candidate<'_>) -> Option<T> + Send,
    {
        use iced::advanced::widget::Operation;
        use iced_test::selector::Selector;
        let mut operation = selector.find();
        self.ui.operate(
            self.renderer,
            &mut iced::advanced::widget::operation::black_box(&mut operation),
        );
        match operation.finish() {
            iced::advanced::widget::operation::Outcome::Some(found) => found,
            _ => None,
        }
    }
}

/// How far the scrollable `id` is scrolled down.
fn scrolled(ui: &mut impl Drive, id: &'static str) -> Option<f32> {
    ui.select(move |candidate| match candidate {
        iced_test::selector::Candidate::Scrollable {
            id: Some(found),
            translation,
            ..
        } if *found == iced::widget::Id::new(id) => Some(translation.y),
        _ => None,
    })
}

/// A screen point that hit-tests to the start of `needle` in the first selectable text that
/// starts with `leaf` (which must be fully on screen).
fn aim_at(ui: &mut impl Drive, leaf: &str, needle: &str) -> iced::Point {
    use iced_test::selector::Candidate;
    let (leaf, needle) = (leaf.to_string(), needle.to_string());
    let what = format!("{needle:?} in a text starting {leaf:?}");
    ui.select(move |candidate| {
        let Candidate::Custom {
            bounds,
            visible_bounds: Some(visible),
            state,
            ..
        } = candidate
        else {
            return None;
        };
        let text = state.downcast_ref::<crate::selection::Selectable>()?;
        if !text.text().starts_with(leaf.as_str()) {
            return None;
        }
        assert!(
            (visible.width - bounds.width).abs() < 0.5
                && (visible.height - bounds.height).abs() < 0.5,
            "{leaf:?} is not all on screen"
        );
        let offset = text.text().find(needle.as_str())?;
        let local = text.layout().caret_point(offset)?;
        let translation = visible.position() - bounds.position();
        Some(text.origin(bounds) + translation + (local - iced::Point::ORIGIN))
    })
    .unwrap_or_else(|| panic!("no {what}"))
}

/// What is highlighted, text by text, in reading order.
fn highlighted(ui: &mut impl Drive) -> Vec<String> {
    let found = Arc::new(Mutex::new(Vec::new()));
    let sink = found.clone();
    let _: Option<()> = ui.select(move |candidate| {
        if let iced_test::selector::Candidate::Custom { state, .. } = candidate
            && let Some(text) = state.downcast_ref::<crate::selection::Selectable>()
            && let Some(part) = text.highlighted_text()
        {
            sink.lock().unwrap().push(part.to_string());
        }
        None
    });
    found.lock().unwrap().clone()
}

fn press_at(ui: &mut impl Drive, at: iced::Point) {
    use iced::mouse::{Button, Event};
    ui.aim(at);
    ui.send(vec![
        iced::Event::Mouse(Event::CursorMoved { position: at }),
        iced::Event::Mouse(Event::ButtonPressed(Button::Left)),
    ]);
}

fn move_to(ui: &mut impl Drive, at: iced::Point) {
    ui.aim(at);
    ui.send(vec![iced::Event::Mouse(iced::mouse::Event::CursorMoved {
        position: at,
    })]);
}

fn release_at(ui: &mut impl Drive, at: iced::Point) {
    use iced::mouse::{Button, Event};
    ui.aim(at);
    ui.send(vec![
        iced::Event::Mouse(Event::CursorMoved { position: at }),
        iced::Event::Mouse(Event::ButtonReleased(Button::Left)),
    ]);
}

fn drag(ui: &mut impl Drive, from: iced::Point, to: iced::Point) {
    press_at(ui, from);
    move_to(ui, to);
    release_at(ui, to);
}

/// Ctrl+C as a keyboard sends it: the modifier, the key, the modifier let go.
fn ctrl_c() -> Vec<iced::Event> {
    use iced::keyboard::{Event, Key, Location, Modifiers, key};
    vec![
        iced::Event::Keyboard(Event::ModifiersChanged(Modifiers::COMMAND)),
        iced::Event::Keyboard(Event::KeyPressed {
            key: Key::Character("c".into()),
            modified_key: Key::Character("c".into()),
            physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::COMMAND,
            text: None,
            repeat: false,
        }),
        iced::Event::Keyboard(Event::ModifiersChanged(Modifiers::empty())),
    ]
}

fn wheel(lines: f32) -> iced::Event {
    iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
        delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: lines },
    })
}

/// What was copied, in order.
fn copies(messages: impl IntoIterator<Item = Message>) -> Vec<String> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::Copy(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// Whether `task` scrolls a widget (the only widget operation tail-follow issues).
fn snaps(task: Task<Message>) -> bool {
    use iced::futures::StreamExt;
    iced_runtime::task::into_stream(task).is_some_and(|stream| {
        block_on(stream.collect::<Vec<_>>())
            .iter()
            .any(|action| matches!(action, iced_runtime::Action::Widget(_)))
    })
}

/// The demo's turn with all of its text streamed: the prompt, the shell call, and the agent's
/// paragraphs, table, code block and list.
fn streamed() -> App {
    let (mut app, _) = App::new(true, None);
    for _ in 0..demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    app
}

/// The gate (docs/sill-gui.md, direction 5): a drag from the person's prompt into the agent's
/// message selects everything between — its paragraphs, the table's cells, the code lines, the
/// list — highlights each, and Ctrl+C copies it in reading order.
#[test]
fn a_drag_across_two_messages_copies_both_in_reading_order() {
    let app = streamed();
    let mut ui = simulator(&app, (720.0, 860.0));
    let from = aim_at(&mut ui, "Make the send arrow", "smaller");
    let to = aim_at(&mut ui, "Code blocks have", "blocks");
    drag(&mut ui, from, to);
    let lit = highlighted(&mut ui);
    assert_eq!(
        lit.first().map(String::as_str),
        Some("smaller and show me the change.")
    );
    assert_eq!(lit.last().map(String::as_str), Some("Code "));
    assert_eq!(lit.len(), 16, "every text between is highlighted: {lit:?}");
    ui.send(ctrl_c());
    assert_eq!(
        copies(ui.into_messages()),
        ["smaller and show me the change.\n\
          The arrow's size is one rule in the stylesheet.\n\
          A streamed paragraph with bold, italic, and inline code — the send arrow is drawn in conversation.rs.\n\
          Check Result\n\
          Markdown Rendered\n\
          Unicode café · 日本語 · 🦀\n\
          fn main() {\n    println!(\"Sill GUI\");\n}\n\
          • Select text within a paragraph, then press Ctrl+C.\n\
          • Code "]
    );
}

/// New text does not pull the conversation to its end from under a held selection drag; the
/// selection made meanwhile survives the text, and following resumes once the drag ends.
#[test]
fn tail_follow_waits_while_a_selection_is_dragged() {
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    let pressed = live.frame(&app, |ui| {
        let from = aim_at(ui, "Make the send arrow", "send");
        press_at(ui, from);
    });
    assert!(
        pressed
            .iter()
            .any(|m| matches!(m, Message::Selecting(true))),
        "{pressed:?}"
    );
    for message in pressed {
        live.apply(&mut app, message);
    }
    assert!(app.selecting);
    assert!(!snaps(app.update(Message::DemoTick)), "a streamed chunk");
    let seq = demo_view(&app).conversation.seq + 1;
    let event = sill_client::lane::Update::Event(Box::new(demo::append(seq, " More.")));
    assert!(
        !snaps(app.update(Message::Lane("demo".into(), 0, event))),
        "an event from the lane"
    );
    assert!(demo_view(&app).follow_tail);
    let released = live.frame(&app, |ui| {
        let to = aim_at(ui, "A streamed paragraph", "streamed");
        move_to(ui, to);
        release_at(ui, to);
        ui.send(ctrl_c());
    });
    assert!(
        released
            .iter()
            .any(|m| matches!(m, Message::Selecting(false))),
        "{released:?}"
    );
    assert_eq!(
        copies(released.clone()),
        ["send arrow a little smaller and show me the change.\n\
          The arrow's size is one rule in the stylesheet.\nA "]
    );
    for message in released {
        live.apply(&mut app, message);
    }
    assert!(!app.selecting);
    assert!(snaps(app.update(Message::DemoTick)), "following resumes");
}

/// A selection in earlier messages stays put, and copies the same, while the agent's text
/// streams in below it and the conversation follows its tail.
#[test]
fn streamed_text_keeps_a_selection_in_earlier_messages() {
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((720.0, 600.0));
    let made = live.frame(&app, |ui| {
        let from = aim_at(ui, "Make the send arrow", "send");
        let to = aim_at(ui, "The arrow's size", "rule");
        drag(ui, from, to);
        ui.send(ctrl_c());
    });
    let copied = copies(made.clone());
    assert_eq!(
        copied,
        ["send arrow a little smaller and show me the change.\nThe arrow's size is one "]
    );
    for message in made {
        live.apply(&mut app, message);
    }
    for _ in 1..=demo::CHUNKS.len() {
        live.apply(&mut app, Message::DemoTick);
        let again = live.frame(&app, |ui| ui.send(ctrl_c()));
        assert_eq!(copies(again), copied);
    }
}

/// Scrolling up from the tail turns following off and puts "↓ Latest" over the conversation;
/// the conversation stays where it was scrolled to (it used to start again at its top).
#[test]
fn scrolling_up_from_the_tail_keeps_the_place() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((720.0, 400.0));
    live.apply(&mut app, Message::Latest);
    let mut at_end = None;
    let scrolled_up = live.frame(&app, |ui| {
        at_end = scrolled(ui, "conversation");
        ui.aim(iced::Point::new(360.0, 200.0));
        ui.send(vec![wheel(1.0)]);
    });
    for message in scrolled_up {
        live.apply(&mut app, message);
    }
    assert!(!demo_view(&app).follow_tail);
    let mut now = None;
    live.frame(&app, |ui| now = scrolled(ui, "conversation"));
    let (at_end, now) = (at_end.unwrap(), now.unwrap());
    assert!(at_end > 100.0, "the conversation is longer than the window");
    assert!(
        (at_end - 60.0 - now).abs() < 1.0,
        "one notch above the end: {now}"
    );
}

/// Regression (found 2026-11-04): opening the Files drawer moved the conversation to another
/// place in the widget tree, which gave its scrollable fresh state — back at the top, any
/// selection in it dropped.
#[test]
fn opening_the_drawer_keeps_the_conversation_s_place() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    // wide enough that the prose keeps its measure beside the drawer: nothing rewraps
    let mut live = Live::new((1300.0, 400.0));
    live.apply(&mut app, Message::Latest);
    let mut before = None;
    live.frame(&app, |ui| before = scrolled(ui, "conversation"));
    live.apply(
        &mut app,
        Message::Layout(crate::layout::Action::Mode(crate::layout::Mode::Drawer)),
    );
    assert!(app.drawer.shown);
    let mut after = None;
    live.frame(&app, |ui| after = scrolled(ui, "conversation"));
    let (before, after) = (before.unwrap(), after.unwrap());
    assert!(before > 100.0, "the conversation is longer than the window");
    assert!(
        (before - after).abs() < 1.0,
        "the place moved when the drawer opened: {before} → {after}"
    );
}

/// A press on a button (a tool's line) is the button's and leaves the selection alone; a click
/// without a drag clears the selection, and Ctrl+C then copies nothing.
#[test]
fn a_button_keeps_its_press_and_a_click_clears_the_selection() {
    let app = streamed();
    let mut ui = simulator(&app, (720.0, 860.0));
    let from = aim_at(&mut ui, "Make the send arrow", "send");
    let to = aim_at(&mut ui, "The arrow's size", "rule");
    drag(&mut ui, from, to);
    ui.click("rg -n 'SEND_SIZE' crates/sill-gui/src").unwrap();
    assert_eq!(highlighted(&mut ui).len(), 2);
    let elsewhere = aim_at(&mut ui, "I'll make the arrow", "arrow");
    press_at(&mut ui, elsewhere);
    release_at(&mut ui, elsewhere);
    assert!(highlighted(&mut ui).is_empty());
    ui.send(ctrl_c());
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ToggleTool(id) if id == "tool")),
        "{messages:?}"
    );
    assert!(copies(messages).is_empty());
}

/// A file in the reader selects the same way: across a Markdown file's blocks, and across a
/// diff's lines without their line numbers.
#[test]
fn the_reader_selects_across_markdown_blocks_and_diff_lines() {
    use crate::files::Action;
    use sill_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_path(std::path::Path::new(s));
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: None,
    }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    let from = aim_at(&mut ui, "Browse pinned folders", "review");
    let to = aim_at(&mut ui, "    println!", "Hello");
    drag(&mut ui, from, to);
    ui.send(ctrl_c());
    assert_eq!(
        copies(ui.into_messages()),
        ["review what changed without leaving the conversation.\n\
          Staying oriented\n\
          • Expanded folders stay open across refreshes.\n\
          • Each session remembers its own files.\n\
          • Close the viewer to return to the conversation.\n\
          fn main() {\n    println!(\""]
    );
    let _ = app.update(Message::Files(Action::DiffMode(true)));
    let _ = app.update(Message::Files(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: Some(path("/home/demo/project")),
    }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    let from = aim_at(&mut ui, "-A conversation preview.", "-A");
    let to = aim_at(&mut ui, "+Files and diffs", "Files");
    drag(&mut ui, from, to);
    ui.send(ctrl_c());
    assert_eq!(
        copies(ui.into_messages()),
        ["-A conversation preview.\n+A place to work with your agents.\n+"]
    );
}

// --- keyboard -----------------------------------------------------------------------------------

/// A key press as the keyboard sends it, with nothing held.
fn key(named: iced::keyboard::key::Named) -> iced::Event {
    use iced::keyboard::{Event, Key, Location, Modifiers, key};
    iced::Event::Keyboard(Event::KeyPressed {
        key: Key::Named(named),
        modified_key: Key::Named(named),
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        location: Location::Standard,
        modifiers: Modifiers::empty(),
        text: None,
        repeat: false,
    })
}

/// Tab `steps` times through `app`, pressing `press` at each stop: what each press asks for
/// (not applied, so the window stays as it is).
fn at_each_stop(
    app: &mut App,
    size: (f32, f32),
    steps: usize,
    press: iced::keyboard::key::Named,
) -> Vec<Option<Message>> {
    let mut live = Live::new(size);
    live.frame(app, |_| {});
    (0..steps)
        .map(|_| {
            live.apply(app, Message::Focus(true));
            live.frame(app, |ui| ui.send(vec![key(press)]))
                .into_iter()
                .next()
        })
        .collect()
}

/// The keyboard gate (docs/sill-gui.md, direction 6): Tab reaches the controls in reading order —
/// the session's tab, the top bar, the file chip, the tool row, the question's options, the
/// prompt, the controls, the shell and Stop — and Enter or Space presses the focused one.
#[test]
fn tab_reaches_every_control_in_reading_order_and_enter_presses_it() {
    use iced::keyboard::key::Named;
    let mut app = streamed();
    let _ = app.update(Message::DemoTick); // the edit now waits on its question
    let pressed = at_each_stop(&mut app, (720.0, 860.0), 40, Named::Enter);
    let names: Vec<String> = pressed
        .iter()
        .map(|m| match m {
            Some(Message::Choose(id)) => format!("choose {id}"),
            Some(Message::Files(action)) => {
                format!("files {action:?}").chars().take(24).collect()
            }
            Some(Message::Answer { option_id, .. }) => format!("answer {option_id}"),
            Some(other) => format!("{other:?}").chars().take(24).collect(),
            None => "-".into(),
        })
        .collect();
    let at = |what: &str| {
        names
            .iter()
            .position(|n| n.starts_with(what))
            .unwrap_or_else(|| panic!("{what} not reached: {names:?}"))
    };
    assert_eq!(names[0], "ToggleChanges", "{names:?}");
    assert_eq!(names[1], "Details(Open(\"demo\"))", "{names:?}");
    assert_eq!(names[2], "choose demo", "{names:?}");
    let order = [
        at("files Toggle("),
        at("ToggleTool(\"tool\")"),
        at("answer allow"),
        at("answer reject"),
        at("Send"),
        at("ToggleTerminal"),
        at("Stop"),
    ];
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "{order:?} in {names:?}"
    );
}

/// Space presses too, and a dropdown steps through its choices with Up and Down.
#[test]
fn space_presses_and_arrows_step_a_dropdown() {
    use iced::keyboard::key::Named;
    let mut app = streamed();
    let spaced = at_each_stop(&mut app, (720.0, 860.0), 3, Named::Space);
    assert!(
        matches!(&spaced[2], Some(Message::Choose(id)) if id == "demo"),
        "{spaced:?}"
    );
    let stepped = at_each_stop(&mut app, (720.0, 860.0), 40, Named::ArrowDown);
    assert!(
        stepped
            .iter()
            .any(|m| matches!(m, Some(Message::SetMode(pick)) if pick.value == "plan")),
        "the mode dropdown takes Down: {stepped:?}"
    );
    assert!(
        stepped.iter().any(|m| matches!(m, Some(Message::SetOption(option, pick)) if option == "model" && pick.value == "brisk")),
        "{stepped:?}"
    );
}

/// Focus moving to a control out of sight scrolls the conversation to it.
#[test]
fn focus_scrolls_the_control_into_sight() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((720.0, 380.0));
    live.frame(&app, |_| {});
    let mut offsets = vec![];
    for _ in 0..12 {
        live.apply(&mut app, Message::Focus(true));
        let mut offset = None;
        live.frame(&app, |ui| offset = scrolled(ui, "conversation"));
        offsets.push(offset.unwrap_or_default());
    }
    assert!(
        offsets.iter().any(|y| *y > 50.0),
        "tabbing into the conversation scrolled it: {offsets:?}"
    );
}

/// Session shortcuts replace the old bindings without taking word selection from editors.
#[test]
fn session_shortcuts_work_from_the_prompt() {
    use crate::keys::shortcut;
    use iced::keyboard::{Event, Key, Location, Modifiers, key, key::Named};
    let press = |key: Key, modifiers| Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        location: Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    };
    let named = |n| Key::Named(n);
    let ch = |c: &str| Key::Character(c.into());
    let both = Modifiers::COMMAND | Modifiers::SHIFT;
    assert!(matches!(
        shortcut(press(named(Named::Tab), Modifiers::empty())),
        Some(Message::Focus(true))
    ));
    assert!(matches!(
        shortcut(press(named(Named::Tab), Modifiers::SHIFT)),
        Some(Message::Focus(false))
    ));
    assert!(matches!(
        shortcut(press(ch("."), Modifiers::COMMAND)),
        Some(Message::StepTab(1))
    ));
    assert!(matches!(
        shortcut(press(ch(","), Modifiers::COMMAND)),
        Some(Message::StepTab(-1))
    ));
    assert!(matches!(
        shortcut(press(ch("F"), both)),
        Some(Message::Layout(crate::layout::Action::ToggleFull))
    ));
    assert!(matches!(
        shortcut(press(ch("t"), Modifiers::COMMAND)),
        Some(Message::NewSession)
    ));
    assert!(matches!(
        shortcut(press(named(Named::Escape), Modifiers::empty())),
        Some(Message::Escape)
    ));
    assert!(matches!(
        shortcut(press(named(Named::PageDown), Modifiers::empty())),
        Some(Message::Page(true))
    ));
    assert!(matches!(
        shortcut(press(named(Named::End), Modifiers::COMMAND)),
        Some(Message::Latest)
    ));
    assert!(matches!(
        shortcut(press(ch("q"), Modifiers::COMMAND)),
        Some(Message::Quit)
    ));
    assert!(shortcut(press(ch("a"), Modifiers::empty())).is_none());

    assert!(matches!(
        shortcut(press(ch("w"), Modifiers::COMMAND)),
        Some(Message::CloseCurrent)
    ));
    for digit in 1..=9 {
        assert!(
            matches!(shortcut(press(ch(&digit.to_string()), Modifiers::COMMAND)),
            Some(Message::SelectTab(at)) if at == digit - 1)
        );
    }
    // Ctrl+0 is no tab: it puts Interface scale back to 100 %, as + and - step it
    for (key, step) in [("0", 0), ("+", 1), ("=", 1), ("-", -1)] {
        assert!(
            matches!(shortcut(press(ch(key), Modifiers::COMMAND)), Some(Message::ScaleStep(s)) if s == step),
            "Ctrl+{key}"
        );
    }
    assert!(matches!(
        shortcut(press(ch("t"), both)),
        Some(Message::ToggleTerminal)
    ));
    assert!(matches!(
        shortcut(press(ch("T"), both)),
        Some(Message::ToggleTerminal)
    ));
    assert!(shortcut(press(ch("t"), Modifiers::COMMAND | Modifiers::ALT)).is_none());
    assert!(shortcut(press(named(Named::ArrowLeft), both)).is_none());
    assert!(shortcut(press(named(Named::ArrowRight), both)).is_none());

    // Editors retain word selection; paging and Tab keep their existing behavior.
    use iced::widget::text_editor::{Binding, KeyPress, Status};
    let typed = |key: Key, modifiers| KeyPress {
        key: key.clone(),
        modified_key: key,
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        modifiers,
        text: None,
        status: Status::Focused { is_hovered: false },
    };
    assert!(matches!(
        crate::ui::enter_sends(typed(named(Named::ArrowLeft), JUMP | Modifiers::SHIFT)),
        Some(Binding::Select(iced::widget::text_editor::Motion::WordLeft))
    ));
    assert!(matches!(
        crate::ui::enter_sends(typed(named(Named::PageUp), Modifiers::empty())),
        Some(Binding::Custom(Message::Page(false)))
    ));
    assert!(crate::ui::enter_sends(typed(named(Named::Tab), Modifiers::empty())).is_none());
}

/// Ctrl+comma/period walk the tabs, round the ends.
#[test]
fn stepping_tabs_wraps_round() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    let _ = app.update(Message::StepTab(1));
    assert_eq!(app.selected.as_deref(), Some("other"));
    let _ = app.update(Message::StepTab(1));
    assert_eq!(app.selected.as_deref(), Some("demo"));
    let _ = app.update(Message::StepTab(-1));
    assert_eq!(app.selected.as_deref(), Some("other"));
}

/// Escape goes between the session and the backend: an open file or menu closes first; the
/// drawer and the mode stay as they are; the shell keeps Escape.
#[test]
fn escape_toggles_session_and_backend() {
    use crate::layout::Mode;
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(Mode::Full)));
    let _ = app.update(Message::Files(Action::ToggleDrawer));
    let _ = app.update(Message::Files(Action::Open {
        path: sill_proto::sessions::HostPath::from_path(std::path::Path::new(
            "/home/demo/project/README.md",
        )),
        repo: None,
    }));
    let _ = app.update(Message::Escape);
    assert!(
        demo_view(&app).files.reader.is_none(),
        "an open file closes first"
    );
    assert!(app.changes.is_none());
    let task = app.update(Message::Escape);
    settle_task(&mut app, task);
    assert!(app.changes.is_some(), "then to the backend's default page");
    let _ = app.update(Message::Escape);
    assert!(app.changes.is_none(), "and back");
    assert!(app.drawer.shown, "the drawer stays");
    assert_eq!(app.layout.mode, Mode::Full, "the mode stays");
    // a menu closes first
    let _ = app.update(Message::Menu(crate::menu::Event::Toggle));
    let _ = app.update(Message::Escape);
    assert!(app.menu.is_none() && app.changes.is_none());
    // the shell keeps it
    app.views.get_mut("demo").unwrap().terminal_shown = true;
    let _ = app.update(Message::Escape);
    assert!(app.changes.is_none(), "the shell has Escape");
}

// --- sessions, forms, held prompts ---------------------------------------------------------------

pub(crate) fn session_record(
    id: &str,
    title: &str,
    ended: bool,
    ghost: bool,
) -> sill_proto::sessions::Session {
    serde_json::from_value(json!({
        "session_id": id, "agent": "claude", "title": title, "title_kind": "",
        "created_at": "1", "last_used": "1", "ended_at": ended.then_some("1793806135"),
        "policy": "default", "auto_allow": false, "memory_scope": "",
        "summary": if ended { "made the arrow smaller" } else { "" }, "summary_kind": "",
        "ghost": ghost, "running": !ended, "pins": [], "settings": {}, "follows": null,
        "compacted": "0"
    }))
    .unwrap()
}

/// The menu shows the agents with their ghost buttons, then the sessions to reopen — the first
/// five and "N more…" — and its rows ask for what they say.
/// The + opens its menu as the pointer comes over it; hovering again keeps it; the pointer
/// leaving the menu closes it; outside presses can still dismiss it.
#[test]
fn the_menu_opens_on_hover_and_closes_when_the_pointer_leaves() {
    use crate::menu::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(Event::Hover));
    assert!(app.menu.is_some());
    let _ = app.update(Message::Menu(Event::Hover));
    assert!(app.menu.is_some(), "hovering an open menu does not close it");
    let _ = app.update(Message::Menu(Event::Leave));
    assert!(app.menu.is_none());
    let _ = app.update(Message::Menu(Event::Toggle));
    assert!(app.menu.is_some());
    let _ = app.update(Message::Menu(Event::Toggle));
    assert!(app.menu.is_none());
}

#[test]
fn a_registry_agents_icon_reaches_the_menu_and_the_tab_once_it_arrives() {
    let (mut app, _) = App::new(true, None);
    // before it arrives: the neutral glyph, no fetch queued for built-ins
    assert_eq!(app.agent_icons_load(vec!["claude".into(), "codex".into()]).units(), 0);
    let fallback = app.agent_icon("antigravity-acp");
    assert_eq!(fallback.id(), crate::icons::for_agent("antigravity-acp").id(), "the neutral glyph until the icon arrives");
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><circle cx="5" cy="5" r="4"/></svg>"#.to_vec();
    let _ = app.update(Message::AgentIcon("antigravity-acp".into(), Some(svg.clone())));
    assert_eq!(app.agent_icons.get("antigravity-acp"), Some(&svg));
    assert_ne!(app.agent_icon("antigravity-acp").id(), fallback.id(), "the registry icon once it arrived");
    let _ = app.update(Message::AgentIcon("other".into(), None));
    assert!(!app.agent_icons.contains_key("other"));
    // the tab texture takes the custom SVG without panicking (a silhouette in the tab colour)
    let _ = crate::icons::tab_agent("antigravity-acp", Some(&svg), crate::style::P.fg, false);
}

#[test]
fn an_agent_without_directory_setup_is_dimmed_in_the_menu_and_opens_the_agents_page() {
    use crate::menu::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(Event::Toggle));
    let agents = vec![
        serde_json::from_value(serde_json::json!({"id": "claude", "name": "Claude Code", "command": ["claude-acp"], "home_paths": [{"path": "~/.claude", "kind": "directory"}]})).unwrap(),
        serde_json::from_value(serde_json::json!({"id": "antigravity-acp", "name": "Antigravity", "command": ["antigravity-acp"]})).unwrap(),
    ];
    app.menu.as_mut().unwrap().agents = Some(Ok(agents));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("needs setup").unwrap();
    ui.click("Antigravity").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::Menu(Event::Setup(id))] if id == "antigravity-acp"), "{messages:?}");
    for m in messages {
        let _ = app.update(m);
    }
    assert!(app.menu.is_none(), "the menu closes");
    assert!(app.agents_page.shown, "the Agents page opens instead of a session");
}

#[test]
fn the_menu_lists_agents_and_sessions_to_reopen() {
    use crate::menu::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(Event::Toggle));
    app.all_sessions = (0..7)
        .map(|i| {
            session_record(
                &format!("old{i}"),
                &format!("Ended session {i}"),
                true,
                false,
            )
        })
        .collect();
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Claude Code").unwrap();
    ui.find("Codex").unwrap();
    ui.find("2 more…").unwrap();
    assert!(ui.find("Ended session 6").is_err(), "only the first five");
    ui.click("Ended session 1").unwrap();
    ui.click("Codex").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::Menu(Event::Reopen(id)), Message::Menu(Event::New { agent, ghost: false, workspaces: None })] if id == "old1" && agent == "codex"),
        "{messages:?}"
    );
    let _ = app.update(Message::Menu(Event::Filter("session 6".into())));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Ended session 6").unwrap();
    assert!(ui.find("Ended session 1").is_err(), "filtered out");
}

/// Starting from the menu creates the session with a fresh identity and opens it once the list
/// shows it; deleting asks for a second press.
#[test]
fn the_menu_starts_a_session_and_opens_it() {
    use crate::menu::Event;
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let server = std::thread::spawn(move || {
        let create = daemon.expect("_sill/sessions.create");
        let params = &create["params"];
        assert_eq!(
            (params["agent"].as_str(), params["ghost"].as_bool()),
            (Some("codex"), Some(true))
        );
        assert_eq!(params["policy"], "default");
        assert!(
            params["create_id"]
                .as_str()
                .unwrap()
                .starts_with("gui-create-")
        );
        daemon.reply(&create, json!({"session_id": "fresh"}));
        let list = daemon.expect("_sill/sessions.list");
        daemon.reply(
            &list,
            json!({"sessions": [session_record("fresh", "new one", false, true),
                                session_record("demo", "witty-willow", false, false)],
                   "next": null}),
        );
        let delete = daemon.expect("_sill/sessions.delete");
        assert_eq!(delete["params"], json!({"session_id": "old"}));
        daemon.reply(&delete, json!({}));
        let list = daemon.expect("_sill/sessions.list");
        daemon.reply(&list, json!({"sessions": [], "next": null}));
    });
    let task = app.update(Message::Menu(Event::New {
        agent: "codex".into(),
        ghost: true,
        workspaces: None,
    }));
    settle_task(&mut app, task);
    assert_eq!(app.selected.as_deref(), Some("fresh"), "opened once listed");
    app.menu = Some(crate::menu::Menu::default());
    let first = app.update(Message::Menu(Event::Delete("old".into())));
    settle_task(&mut app, first);
    assert_eq!(app.menu.as_ref().unwrap().armed.as_deref(), Some("old"));
    let second = app.update(Message::Menu(Event::Delete("old".into())));
    settle_task(&mut app, second);
    server.join().unwrap();
}

/// The selected tab's × ends its session; a ghost's asks once first, since closing deletes it.
#[test]
fn closing_a_tab_ends_its_session_and_a_ghost_asks_first() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.all_sessions = vec![session_record("demo", "witty-willow", false, true)];
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("close-tab")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::CloseTab(id)] if id == "demo"),
        "{messages:?}"
    );
    let _ = app.update(Message::CloseTab("demo".into()));
    assert_eq!(app.armed_close.as_deref(), Some("demo"));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("close-tab")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::CloseTab(id)] if id == "demo"));
    let server = std::thread::spawn(move || {
        let end = daemon.expect("_sill/sessions.end");
        assert_eq!(
            end["params"],
            json!({"session_id": "demo", "reason": "close"})
        );
        daemon.reply(&end, json!({"pending": false, "deleted": true}));
        let list = daemon.expect("_sill/sessions.list");
        daemon.reply(&list, json!({"sessions": [], "next": null}));
    });
    let task = app.update(Message::CloseTab("demo".into()));
    settle_task(&mut app, task);
    assert!(
        app.tabs.is_empty() && app.selected.is_none(),
        "the ended session's tab went"
    );
    server.join().unwrap();
}

/// A form question is answered in place: its fields, then Submit with the values as `forms`
/// encodes them; Decline cancels.
#[test]
fn a_form_question_is_answered_with_its_values() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let view = app.views.get_mut("demo").unwrap();
    let question: Question = serde_json::from_value(json!({
        "question_id":"form", "agent_generation_id":"demo-generation", "kind":"form",
        "title":"Deploy where?", "options":[], "state":"open",
        "fields":[
            {"key":"env", "title":"Environment", "description":"", "kind":"choice",
             "choices":[{"value":"stage","title":"Staging","description":""},
                        {"value":"prod","title":"Production","description":""}]},
            {"key":"why", "title":"Why", "description":"", "kind":"text", "choices":[]}
        ]
    }))
    .unwrap();
    let next = event(&view.conversation, Event::QuestionUpdated { question });
    assert!(view.accept(next));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Deploy where?").unwrap();
    ui.click("Production").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    let _ = app.update(Message::FormEdit(
        "form".into(),
        crate::forms::Edit::Text {
            key: "why".into(),
            text: "release day".into(),
        },
    ));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("Submit").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::SubmitForm(q)] if q == "form"),
        "{messages:?}"
    );
    let task = app.update(Message::SubmitForm("form".into()));
    let answer = daemon.expect("_sill/questions.answer");
    assert_eq!(
        answer["params"]["answer"],
        json!({"type":"form", "values":{"env":"prod", "why":"release day"}})
    );
    serde_json::from_value::<AnswerQuestion>(answer["params"].clone()).unwrap();
    daemon.reply(&answer, json!({}));
    settle_task(&mut app, task);
}

/// A held prompt shows above the composer with Send and Discard, which resume or cancel it.
#[test]
fn a_held_prompt_can_be_sent_or_discarded() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let view = app.views.get_mut("demo").unwrap();
    let submission: Submission = serde_json::from_value(json!({
        "submission_id":"held1", "session_id":"demo", "accepted_seq":"4",
        "agent_generation_id":"demo-generation", "text":"the prompt from before", "attachments":[],
        "acceptance":"accepted", "delivery":"queued", "held":true,
        "hold_reason":{"code":"restart", "message":"the agent restarted"},
        "insertion":"pending", "outcome":"pending"
    }))
    .unwrap();
    let next = event(&view.conversation, Event::SubmissionUpdated { submission });
    assert!(view.accept(next));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Held: the agent restarted").unwrap();
    ui.click("Send").unwrap();
    ui.click("Discard").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::ResumeHeld(a), Message::DiscardHeld(b)] if a == "held1" && b == "held1"),
        "{messages:?}"
    );
    let task = app.update(Message::ResumeHeld("held1".into()));
    let resume = daemon.expect("_sill/submissions.resume");
    assert_eq!(
        resume["params"],
        json!({"session_id":"demo", "submission_id":"held1"})
    );
    daemon.reply(&resume, json!({}));
    settle_task(&mut app, task);
}

// --- changes ----------------------------------------------------------------------------------

fn changes_key(app: &App, file: &str) -> String {
    let entries = app.changes_listing.entries.as_ref().unwrap().as_ref().unwrap();
    entries
        .iter()
        .find(|e| sill_client::changes::path(e).display.ends_with(file))
        .map(sill_client::changes::entry_key)
        .unwrap()
}

/// The herring opens the Changes page: the session's section with its writes (one changed on the
/// host, which cannot be kept) and an earlier keep to undo, then the person's checkout as git
/// sees it. A row opens its diff; reverting takes a second press; Escape goes back.
#[test]
fn the_changes_page_groups_what_sessions_wrote() {
    use crate::changes_ui::Event;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let mut ui = simulator(&app, (720.0, 860.0));
    for shown in [
        "3 writes to keep or revert",
        "witty-willow",
        "Outside its pinned folders",
        "no git",
        "Keep all",
        "settings.toml",
        "/home/demo/.config/app",
        "changed on the host too",
        "Kept",
        "Undo",
        "/home/demo/project",
        "your checkout",
    ] {
        ui.find(shown)
            .unwrap_or_else(|_| panic!("{shown} is not shown"));
    }
    ui.click("settings.toml").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    for message in messages {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("+theme = \"dracula\"\n").unwrap();
    drop(ui);
    let key = changes_key(&app, "settings.toml");
    let _ = app.update(Message::Changes(Event::Revert(key.clone())));
    assert_eq!(
        app.changes.as_ref().unwrap().armed.as_deref(),
        Some(key.as_str())
    );
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Revert?").unwrap();
    drop(ui);
    let _ = app.update(Message::Escape);
    assert!(
        app.changes.is_none(),
        "Escape goes back to the conversation"
    );
}

fn page_workspace(id: &str, session: &str, files: &[&str]) -> sill_proto::workspaces::Detail {
    let host = |p: &str| sill_proto::sessions::HostPath::from_path(std::path::Path::new(p));
    let listed: Vec<serde_json::Value> = files
        .iter()
        .map(|f| json!({"path": host(&format!("/home/demo/project/{f}")), "status": "M", "added": "2", "removed": "1"}))
        .collect();
    serde_json::from_value(json!({
        "workspace": {"workspace_id": id, "name": "project", "session_id": session, "source": host("/home/demo/project"),
            "source_branch": "main", "base": "b0", "branch": format!("sill/{id}"), "path": host(&format!("/ws/{id}")),
            "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"},
        "revision": {"tip": "t1", "worktree": "w", "destination_branch": "main", "destination_tip": "b0"},
        "summary": {"commits": 1, "files": files.len(), "added": "2", "removed": "1", "uncommitted": []},
        "needs_rebase": false, "files": listed}))
    .unwrap()
}

fn page_workspaces(details: Vec<sill_proto::workspaces::Detail>) -> crate::workspaces::All {
    let listed = serde_json::from_value(json!({
        "workspaces": details.iter().map(|d| json!(d.workspace)).collect::<Vec<_>>(), "discarded": [], "next": null}))
    .unwrap();
    (listed, details)
}

/// A session's workspace and its writes outside its pins are one section, the workspace with the
/// files an Apply would bring; a file's row opens what Apply would bring of it, and a workspace
/// that moved has its open diff read again while one whose file went closes it.
#[test]
fn the_changes_page_is_by_session_with_each_workspaces_files() {
    use crate::changes_ui::{Event, workspace_key};
    use crate::workspaces::Event as W;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let all = page_workspaces(vec![page_workspace("w1", "demo", &["src/a.rs", "src/b.rs"])]);
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(all))));
    let mut ui = simulator(&app, (720.0, 860.0));
    for shown in ["1 workspace to apply · 3 writes to keep or revert", "workspace", "2 files +2 −1 · 1 commit", "Apply", "Discard", "a.rs", "b.rs", "+2 −1"] {
        ui.find(shown).unwrap_or_else(|_| panic!("{shown} is not shown"));
    }
    assert!(ui.find("Workspaces").is_err(), "no list of workspaces apart from their sessions");
    ui.click("a.rs").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    for message in messages {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    let host = sill_proto::sessions::HostPath::from_path(std::path::Path::new("/home/demo/project/src/a.rs"));
    let key = workspace_key("w1", &host);
    assert!(matches!(app.changes.as_ref().unwrap().open.get(&key), Some(Some(Ok(_)))), "the file's diff is open");
    // the entries read again: a workspace's open diff is not one of theirs to close
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), 0)))));
    assert!(app.changes.as_ref().unwrap().open.contains_key(&key));
    // the workspace no longer changes that file
    let all = page_workspaces(vec![page_workspace("w1", "demo", &["src/b.rs"])]);
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(all))));
    assert!(!app.changes.as_ref().unwrap().open.contains_key(&key), "its diff went with it");
}

/// The listing is the window's: closing the page keeps it, and opening it again shows it at once
/// while the new one is read — without its actions, which wait for what is there now. A listing
/// that arrives after the page closed is kept for the next opening.
#[test]
fn reopening_changes_shows_the_last_listing_until_the_new_one_is_read() {
    use crate::changes_ui::Event;
    use crate::workspaces::Event as W;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let all = page_workspaces(vec![page_workspace("w1", "demo", &["src/a.rs"])]);
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(all))));
    let page = app.changes.as_ref().unwrap();
    assert!(page.fresh_entries && page.fresh_workspaces);
    let _ = app.update(Message::ToggleChanges);
    assert!(app.changes.is_none());
    // arriving late: still the window's
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes()[..3].to_vec(), 0)))));
    assert_eq!(app.changes_listing.entries.as_ref().unwrap().as_ref().unwrap().len(), 3);
    // open again, the new listing not read yet (the task is not run)
    let _ = app.update(Message::ToggleChanges);
    let page = app.changes.as_ref().unwrap();
    assert!(page.loading && !page.fresh_entries && !page.fresh_workspaces);
    let mut ui = simulator(&app, (720.0, 860.0));
    for shown in ["settings.toml", "a.rs", "Apply"] {
        ui.find(shown).unwrap_or_else(|_| panic!("{shown} is shown from the last listing"));
    }
    assert!(ui.find("Reading changes…").is_err());
    drop(ui);
    // nothing acts on the listing from before
    let key = changes_key(&app, "settings.toml");
    let _ = app.update(Message::Changes(Event::Revert(key)));
    assert_eq!(app.changes.as_ref().unwrap().armed, None);
    let _ = app.update(Message::Workspaces(W::PageDiscard("w1".into())));
    assert_eq!(app.changes.as_ref().unwrap().workspace_armed, None);
    // the new one is read: its actions are offered
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), 0)))));
    let key = changes_key(&app, "settings.toml");
    let _ = app.update(Message::Changes(Event::Revert(key.clone())));
    assert_eq!(app.changes.as_ref().unwrap().armed.as_deref(), Some(key.as_str()));
}

/// The page's actions are the daemon's requests: keep with the version shown, revert all on a
/// second press, git's revert for a live file, undo for a keep; the page reads again after each.
#[test]
fn changes_actions_are_the_public_requests() {
    use crate::changes_ui::{ChangesPage, Event};
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.changes = Some(ChangesPage {
        fresh_entries: true,
        ..ChangesPage::default()
    });
    app.changes_listing.entries = Some(Ok(demo::changes()));
    let keep = changes_key(&app, "settings.toml");
    let live = changes_key(&app, "README.md");
    let kept = changes_key(&app, ".bashrc");
    let server = std::thread::spawn(move || {
        let reload = |daemon: &mut Daemon| {
            // the changes, the settings and the page's workspaces (docs/parallel-coding.md §12)
            for _ in 0..3 {
                let request = daemon.next();
                match request["method"].as_str() {
                    Some("_sill/changes.list") => {
                        daemon.reply(&request, json!({"entries": [], "next": null}))
                    }
                    Some("_sill/workspaces.list") => {
                        daemon.reply(&request, json!({"workspaces": [], "discarded": [], "next": null}))
                    }
                    Some("_sill/settings.get") => daemon.reply(
                        &request,
                        json!({"theme": "dracula", "auto_revert_minutes": 1440}),
                    ),
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        let request = daemon.expect("_sill/changes.keep");
        assert_eq!(request["params"]["session_id"], "demo");
        assert_eq!(request["params"]["items"][0]["expected_hash"], "demo-hash");
        let path = request["params"]["items"][0]["path"].clone();
        daemon.reply(
            &request,
            json!({"done": [{"path": path, "message": ""}], "failed": []}),
        );
        reload(&mut daemon);
        let request = daemon.expect("_sill/changes.revert");
        assert_eq!(request["params"]["paths"].as_array().unwrap().len(), 3);
        daemon.reply(&request, json!({"done": [], "failed": []}));
        reload(&mut daemon);
        let request = daemon.expect("_sill/changes.git-revert");
        assert!(
            request["params"]["path"]["display"]
                .as_str()
                .unwrap()
                .ends_with("README.md")
        );
        daemon.reply(&request, json!({"text": "reverted"}));
        reload(&mut daemon);
        let request = daemon.expect("_sill/changes.undo");
        assert_eq!(
            request["params"],
            json!({"session_id":"demo", "keep_ids":["k1"], "force": false})
        );
        daemon.reply(&request, json!({"done": [], "failed": []}));
        reload(&mut daemon);
    });
    let task = app.update(Message::Changes(Event::Keep(keep)));
    settle_task(&mut app, task);
    let page = app.changes.as_ref().unwrap();
    assert_eq!(page.said.as_deref(), Some("kept: settings.toml"));
    assert_eq!(app.changes_listing.auto_revert, 1440);
    // the page read again (empty now); the rest act on the demo's entries
    app.changes_listing.entries = Some(Ok(demo::changes()));
    let all: Vec<String> = ["settings.toml", "plan.md", ".profile"]
        .iter()
        .map(|f| changes_key(&app, f))
        .collect();
    for _ in 0..2 {
        let task = app.update(Message::Changes(Event::RevertAll(all.clone())));
        settle_task(&mut app, task);
    }
    app.changes_listing.entries = Some(Ok(demo::changes()));
    for _ in 0..2 {
        let task = app.update(Message::Changes(Event::Revert(live.clone())));
        settle_task(&mut app, task);
    }
    app.changes_listing.entries = Some(Ok(demo::changes()));
    let task = app.update(Message::Changes(Event::Undo(kept)));
    settle_task(&mut app, task);
    server.join().unwrap();
}

/// Cancellation keeps delivery=queued in SFP; the outcome removes the composer row.
#[test]
fn cancelled_queued_prompts_disappear_from_the_composer() {
    let (mut app, _) = App::new(true, None);
    let submission: Submission = serde_json::from_value(json!({
        "submission_id":"queued", "session_id":"demo", "accepted_seq":"1",
        "agent_generation_id":"demo-generation", "text":"unique queued prompt", "attachments":[],
        "acceptance":"accepted", "delivery":"queued", "held":false,
        "insertion":"pending", "outcome":"pending"
    }))
    .unwrap();
    app.current_mut()
        .unwrap()
        .conversation
        .submissions
        .insert("queued".into(), submission.clone());
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("unique queued prompt").unwrap();
    ui.find("Queued 1").unwrap();
    drop(ui);
    let mut cancelled = submission;
    cancelled.outcome = "cancelled".into();
    let view = app.current_mut().unwrap();
    let update = event(
        &view.conversation,
        Event::SubmissionUpdated {
            submission: cancelled,
        },
    );
    assert!(view.accept(update));
    let mut ui = simulator(&app, (720.0, 860.0));
    assert!(ui.find("unique queued prompt").is_err());
    assert!(ui.find("Queued 1").is_err());
}

#[test]
fn sending_restores_keyboard_focus_to_the_prompt() {
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    live.frame(&app, |_| {});
    type_text(&mut app, "first prompt");
    live.apply(&mut app, Message::Send);
    type_text(&mut app, "next prompt");
    let messages = live.frame(&app, |ui| {
        ui.send(vec![key(iced::keyboard::key::Named::Enter)])
    });
    assert!(
        messages.iter().any(|m| matches!(m, Message::Send)),
        "{messages:?}"
    );
}

/// Rebuilding the session after a backend page must put the keyboard in its prompt.
#[test]
fn returning_from_backend_restores_keyboard_focus_to_the_prompt() {
    use crate::Page;
    for page in [Page::Changes, Page::Settings, Page::Agents] {
        for back in [Message::Escape, Message::ToggleChanges] {
            let (mut app, _) = App::new(true, None);
            let mut live = Live::new((720.0, 860.0));
            live.frame(&app, |_| {});
            live.apply(&mut app, Message::ShowPage(page));
            live.frame(&app, |_| {});
            live.apply(&mut app, back);
            assert!(!app.backend_shown());
            type_text(&mut app, "next prompt");
            let messages = live.frame(&app, |ui| {
                ui.send(vec![key(iced::keyboard::key::Named::Enter)])
            });
            assert!(
                messages.iter().any(|m| matches!(m, Message::Send)),
                "returning from {page:?}: {messages:?}"
            );
        }
    }
}

/// Crossing the timer and its gap towards + must not collapse the timer and move the button.
#[test]
fn the_timer_stays_open_when_moving_towards_the_plus_button() {
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    app.daemon_settings = Some(sill_proto::admin::Settings {
        timer_auto: Some(false),
        ..Default::default()
    });
    // Approach + directly, without passing over the indicator: it must stay put.
    let messages = live.frame(&app, |ui| move_to(ui, iced::Point::new(670.0, 18.0)));
    for message in messages {
        let _ = app.update(message);
    }
    assert!(!app.activity_hovered, "+ must not open the timer");
    for x in (625..=700).rev() {
        let messages = live.frame(&app, |ui| move_to(ui, iced::Point::new(x as f32, 18.0)));
        for message in messages {
            let _ = app.update(message);
        }
        assert!(app.activity_hovered, "timer closed at x={x}");
    }
    let messages = live.frame(&app, |ui| move_to(ui, iced::Point::new(500.0, 100.0)));
    for message in messages {
        let _ = app.update(message);
    }
    assert!(
        !app.activity_hovered,
        "timer closes after leaving the controls"
    );
}

/// Opening and closing + must retain both the tail and a manually scrolled position.
#[test]
fn the_plus_menu_keeps_the_conversation_position() {
    for at_tail in [true, false] {
        let mut app = streamed();
        let _ = app.update(Message::DemoTick);
        let mut live = Live::new((720.0, 400.0));
        live.apply(&mut app, Message::Latest);
        live.frame(&app, |_| {});
        if !at_tail {
            let messages = live.frame(&app, |ui| {
                ui.aim(iced::Point::new(360.0, 200.0));
                ui.send(vec![wheel(1.0)]);
            });
            for message in messages {
                live.apply(&mut app, message);
            }
        }
        let mut before = None;
        live.frame(&app, |ui| before = scrolled(ui, "conversation"));
        let before = before.unwrap();
        assert!(before > 100.0);
        for open in [true, false] {
            live.apply(&mut app, Message::Menu(crate::menu::Event::Toggle));
            assert_eq!(app.menu.is_some(), open);
            let mut after = None;
            live.frame(&app, |ui| after = scrolled(ui, "conversation"));
            assert!(
                (before - after.unwrap()).abs() < 1.0,
                "menu open={open}, at tail={at_tail}: {before} → {after:?}"
            );
        }
    }
}

#[test]
fn folder_chips_show_tracked_lines_and_untracked_files_independently() {
    use sill_client::files::{Repo, Root, key};
    use sill_proto::sessions::HostPath;
    let (mut app, _) = App::new(true, None);
    let path = HostPath::from_path(std::path::Path::new("/project"));
    let view = app.views.get_mut("demo").unwrap();
    view.files.roots = vec![Root {
        path: path.clone(),
        global: false,
        metadata: None,
        error: None,
    }];
    view.files.repos.insert(
        key(&path).into(),
        Repo {
            root: Some(path.clone()),
            lines: Some(("0".into(), "0".into())),
            summary: "+0 −0  ?3".into(),
            ..Repo::default()
        },
    );
    {
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("?3")
            .expect("new files remain visible with no tracked line changes");
        assert!(ui.find("+0").is_err());
        assert!(ui.find("−0").is_err());
    }
    let repo = app
        .views
        .get_mut("demo")
        .unwrap()
        .files
        .repos
        .get_mut(key(&path))
        .unwrap();
    repo.lines = Some(("12".into(), "7".into()));
    repo.summary = "+12 −7  ?0".into();
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("+12").unwrap();
    ui.find("−7").unwrap();
    assert!(ui.find("?0").is_err());
}

fn hotkey(key: iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Vec<iced::Event> {
    use iced::keyboard::{Event, Location, key::NativeCode, key::Physical};
    vec![
        iced::Event::Keyboard(Event::ModifiersChanged(modifiers)),
        iced::Event::Keyboard(Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }),
    ]
}

#[test]
fn session_shortcuts_precede_text_inputs_and_do_not_insert_characters() {
    use iced::keyboard::{Key, Modifiers};
    let input = iced::widget::text_input("", "alpha beta gamma")
        .id("edit")
        .on_input(Message::Copy);
    let view = crate::keys::intercept_shortcuts(input);
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (600.0, 100.0), view);
    ui.click(iced::widget::Id::new("edit")).unwrap();
    let mut events = hotkey(Key::Character("2".into()), Modifiers::COMMAND);
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut events[1] {
        *text = Some("2".into()); // even backends supplying printable Ctrl text cannot insert it
    }
    ui.send(events);
    ui.send(hotkey(Key::Character("t".into()), Modifiers::COMMAND));
    ui.send(hotkey(
        Key::Character("T".into()),
        Modifiers::COMMAND | Modifiers::SHIFT,
    ));
    ui.send(hotkey(Key::Character("w".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character(",".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character(".".into()), Modifiers::COMMAND));
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        matches!(
            messages.as_slice(),
            [
                Message::SelectTab(1),
                Message::NewSession,
                Message::ToggleTerminal,
                Message::CloseCurrent,
                Message::StepTab(-1),
                Message::StepTab(1)
            ]
        ),
        "{messages:?}"
    );
}

#[test]
fn word_selection_works_in_prompt_and_source_bindings() {
    use iced::{
        keyboard::{Key, Modifiers, key::Named},
        widget::text_editor::{Binding, KeyPress, Motion, Status},
    };
    for binding in [
        crate::ui::enter_sends,
        crate::ui::reading_keys,
        Binding::<Message>::from_key_press,
    ] {
        for (key, motion) in [
            (Named::ArrowLeft, Motion::WordLeft),
            (Named::ArrowRight, Motion::WordRight),
        ] {
            let event = |modifiers| KeyPress {
                key: Key::Named(key),
                modified_key: Key::Named(key),
                physical_key: iced::keyboard::key::Physical::Unidentified(
                    iced::keyboard::key::NativeCode::Unidentified,
                ),
                modifiers,
                text: None,
                status: Status::Focused { is_hovered: false },
            };
            assert!(
                matches!(binding(event(JUMP | Modifiers::SHIFT)), Some(Binding::Select(m)) if m == motion)
            );
            assert!(
                matches!(binding(event(JUMP)), Some(Binding::Move(m)) if m == motion)
            );
        }
    }
    let (mut app, _) = App::new(true, None);
    type_text(&mut app, "alpha beta gamma");
    let mut live = Live::new((720.0, 860.0));
    live.operations.push(Box::new(
        iced::advanced::widget::operation::focusable::focus(iced::widget::Id::new("prompt")),
    ));
    let messages = live.frame(&app, |ui| {
        ui.send(hotkey(Key::Named(Named::Home), Modifiers::empty()));
        ui.send(hotkey(
            Key::Named(Named::ArrowRight),
            JUMP | Modifiers::SHIFT,
        ));
    });
    for message in messages {
        let _ = app.update(message);
    }
    assert_eq!(demo_view(&app).draft.selection().as_deref(), Some("alpha"));
    assert_eq!(app.selected.as_deref(), Some("demo"));
    for (direction, expected) in [
        (Named::ArrowRight, "alpha beta"),
        (Named::ArrowLeft, "alpha "),
    ] {
        let messages = live.frame(&app, |ui| {
            ui.send(hotkey(
                Key::Named(direction),
                JUMP | Modifiers::SHIFT,
            ));
        });
        for message in messages {
            let _ = app.update(message);
        }
        assert_eq!(demo_view(&app).draft.selection().as_deref(), Some(expected));
        assert_eq!(app.selected.as_deref(), Some("demo"));
    }
}

#[test]
fn word_selection_works_in_single_line_inputs() {
    use iced::keyboard::{Key, Modifiers, key::Named};
    let input = iced::widget::text_input("", "alpha beta gamma")
        .id("edit")
        .on_input(Message::Copy);
    let mut ui = iced_test::Simulator::with_size(
        crate::snapshots::settings(),
        (600.0, 100.0),
        crate::keys::intercept_shortcuts(input),
    );
    ui.click(iced::widget::Id::new("edit")).unwrap();
    ui.send(hotkey(Key::Named(Named::Home), Modifiers::empty()));
    ui.send(hotkey(
        Key::Named(Named::ArrowRight),
        JUMP | Modifiers::SHIFT,
    ));
    let mut insert = hotkey(Key::Character("X".into()), Modifiers::empty());
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut insert[1] {
        *text = Some("X".into());
    }
    ui.send(insert);
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        matches!(messages.as_slice(), [Message::Copy(text)] if text == "X beta gamma"),
        "{messages:?}"
    );
}

#[test]
fn numbered_tabs_ignore_missing_positions_and_new_session_opens_chooser_when_empty() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    let _ = app.update(Message::SelectTab(1));
    assert_eq!(app.selected.as_deref(), Some("other"));
    let _ = app.update(Message::SelectTab(8));
    assert_eq!(app.selected.as_deref(), Some("other"));
    let _ = app.update(Message::SelectTab(0));
    assert_eq!(app.selected.as_deref(), Some("demo"));
    app.tabs.clear();
    app.selected = None;
    let _ = app.update(Message::CloseCurrent);
    let _ = app.update(Message::StepTab(1));
    let _ = app.update(Message::NewSession);
    assert!(app.menu.is_some());
}

#[test]
fn session_hotkeys_create_with_current_agent_and_close_current_session() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.tabs[0].agent = "codex".into();
    app.daemon_settings = Some(serde_json::from_value(json!({"remember_pins":true})).unwrap());
    let server = std::thread::spawn(move || {
        let req = daemon.expect("_sill/sessions.create");
        assert_eq!(req["params"]["agent"], "codex");
        assert_eq!(req["params"]["remember_pins"], true);
        daemon.reply(&req, json!({"session_id":"fresh"}));
        let req = daemon.expect("_sill/sessions.list");
        daemon.reply(
            &req,
            json!({"sessions":[session_record("fresh", "Fresh", false, false)], "next":null}),
        );
        let req = daemon.expect("_sill/sessions.end");
        assert_eq!(req["params"]["session_id"], "fresh");
        daemon.reply(&req, json!({}));
        let req = daemon.expect("_sill/sessions.list");
        daemon.reply(&req, json!({"sessions":[], "next":null}));
    });
    let task = app.update(Message::NewSession);
    settle_task(&mut app, task);
    assert_eq!(app.selected.as_deref(), Some("fresh"));
    let task = app.update(Message::CloseCurrent);
    settle_task(&mut app, task);
    assert!(app.tabs.is_empty());
    server.join().unwrap();
}

/// Interface scale grows the content, not the window: the window's and the monitor's sizes are
/// held in the scale's units, so a change converts them; what each display keeps is device pixels
/// over device pixels, so a change leaves it as it is.
#[test]
fn interface_scale_keeps_the_window_and_what_the_displays_keep() {
    let near = |a: f32, b: f32| (a - b).abs() < 0.01;
    let (mut app, _) = App::new(true, None);
    app.width = 500.0;
    app.height = 800.0;
    app.monitor = Some(iced::Size::new(1440.0, 900.0));
    let kept = crate::settings::DisplayPrefs {
        shelf_width: Some(0.3),
        work_width: Some(0.6),
        ..Default::default()
    };
    app.settings.displays.insert("DP-1".into(), kept.clone());
    let _ = app.update(Message::ScaleStep(1));
    assert_eq!(app.settings.scale(), 1.1);
    let _ = app.update(Message::ScaleStep(1));
    assert_eq!(app.settings.scale(), 1.25);
    assert!(
        near(app.width, 400.0) && near(app.height, 640.0),
        "500 × 800 at 100 % in 125 % units"
    );
    let monitor = app.monitor.unwrap();
    assert!(
        near(monitor.width, 1152.0) && near(monitor.height, 720.0),
        "{monitor:?}"
    );
    assert_eq!(app.settings.displays["DP-1"], kept);
    let _ = app.update(Message::ScaleStep(0));
    assert!(near(app.width, 500.0) && near(app.height, 800.0));
    assert_eq!(app.settings.displays["DP-1"], kept);
}

#[test]
fn configured_shortcuts_intercept_inputs_and_release_old_bindings() {
    use iced::keyboard::{Key, Modifiers};
    let bindings = crate::shortcuts::Bindings::from([
        ("new_session".into(), "Alt+N".into()),
        ("close_session".into(), String::new()),
    ]);
    let input = iced::widget::text_input("", "")
        .id("custom-edit")
        .on_input(Message::Copy);
    let view = crate::keys::configured_shortcuts(input, &bindings);
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (600.0, 100.0), view);
    ui.click(iced::widget::Id::new("custom-edit")).unwrap();
    ui.send(hotkey(Key::Character("n".into()), Modifiers::ALT));
    ui.send(hotkey(Key::Character("t".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character("w".into()), Modifiers::COMMAND));
    let messages: Vec<_> = ui.into_messages().collect();
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::NewSession))
            .count(),
        1
    );
    assert!(!messages.iter().any(|m| matches!(m, Message::CloseCurrent)));
    assert!(crate::keys::from_text(&Key::Character("t".into()), Modifiers::COMMAND, true).is_none());
}

#[test]
fn policy_errors_remain_visible_with_the_group_collapsed() {
    let (mut app, _) = App::new(true, None);
    let mut page = crate::settings_ui::SettingsPage::default();
    page.snapshot = Some(Ok(sill_client::settings::Snapshot::default()));
    page.category = crate::settings_ui::Category::AgentBehaviour;
    page.policies.status = "Policy validation failed: invalid TOML".into();
    let view = app.settings_view(&page);
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 5000.0), view);
    ui.find("Policies").unwrap();
    ui.find("Policy validation failed: invalid TOML").unwrap();
    assert!(ui.find("Import policy").is_err());
    drop(ui);
    page.collapsed.remove("Policies");
    app.settings_page = Some(page);
    let view = app.settings_view(app.settings_page.as_ref().unwrap());
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 5000.0), view);
    ui.find("Import policy").unwrap();
    ui.find("Check").unwrap();
}

#[test]
fn host_and_jail_shells_keep_separate_panes_and_route_hidden_events() {
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::ToggleTerminal);
    let jail_generation = app.current().unwrap().terminal_generation;
    assert!(app.current().unwrap().terminal.is_some());
    let _ = app.update(Message::ToggleHostTerminal);
    let host_generation = app.current().unwrap().terminal_generation;
    let id = app.current().unwrap().id.clone();
    assert!(host_generation > jail_generation);
    assert!(app.current().unwrap().terminal_host);
    assert!(app.current().unwrap().parked_terminal.is_some());
    let _ = app.update(Message::Terminal(
        id,
        jail_generation,
        crate::terminal::Event::Ended,
    ));
    assert_eq!(
        app.current()
            .unwrap()
            .parked_terminal
            .as_ref()
            .unwrap()
            .0
            .status,
        crate::terminal::Status::Ended
    );
    let _ = app.update(Message::ToggleHostTerminal);
    assert!(!app.current().unwrap().terminal_shown);
    let _ = app.update(Message::ToggleHostTerminal);
    assert_eq!(app.current().unwrap().terminal_generation, host_generation);
    let _ = app.update(Message::ToggleTerminal);
    assert!(!app.current().unwrap().terminal_host);
    assert!(
        app.current().unwrap().terminal_generation > host_generation,
        "ended jail shell is reopened without reusing host generation"
    );
    let _ = app.update(Message::ToggleHostTerminal);
    assert_eq!(app.current().unwrap().terminal_generation, host_generation);
}

#[test]
fn host_shell_shortcut_is_configurable() {
    use iced::keyboard::{Key, Modifiers};
    let mut bindings = crate::shortcuts::Bindings::new();
    let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
    assert!(matches!(
        crate::shortcuts::message(&bindings, &Key::Character("h".into()), modifiers),
        Some(Message::ToggleHostTerminal)
    ));
    bindings.insert("host_shell".into(), "Alt+H".into());
    assert!(crate::shortcuts::message(&bindings, &Key::Character("h".into()), modifiers).is_none());
    assert!(matches!(
        crate::shortcuts::message(&bindings, &Key::Character("h".into()), Modifiers::ALT),
        Some(Message::ToggleHostTerminal)
    ));
}

#[test]
fn attachment_only_send_keeps_files_on_refusal() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let attachment = sill_proto::sfp::Attachment::Uploaded {
        upload_id: "document".into(),
        name: "report.pdf".into(),
        mime: "application/pdf".into(),
    };
    app.views
        .get_mut("demo")
        .unwrap()
        .attachments
        .push(attachment.clone().into());
    let task = app.update(Message::Send);
    let request = daemon.expect("_sill/sessions.submit");
    let submit: Submit = serde_json::from_value(request["params"].clone()).unwrap();
    assert!(submit.text.is_empty());
    assert_eq!(submit.attachments, vec![attachment.clone()]);
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).attachments.is_empty());
    let view = app.views.get_mut("demo").unwrap();
    let submit = view
        .outbox
        .send_with_attachments("demo", String::new(), vec![attachment.clone()]);
    let _ = app.update(Message::Done(
        "demo".into(),
        crate::Act::Send(submit.submission_id),
        sill_client::actions::Outcome::NotSent("offline".into()),
    ));
    assert_eq!(demo_view(&app).attachments.iter().map(|a| a.attachment.clone()).collect::<Vec<_>>(), vec![attachment]);
}

#[test]
fn attachment_completion_stays_with_originating_session() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    app.selected = Some("other".into());
    let a = sill_proto::sfp::Attachment::Uploaded {
        upload_id: "file".into(),
        name: "notes.txt".into(),
        mime: "text/plain".into(),
    };
    let _ = app.update(Message::Media(crate::media::Event::Attached(
        "demo".into(),
        Ok(vec![(a.clone(), None)]),
    )));
    assert_eq!(app.views["demo"].attachments.iter().map(|a| a.attachment.clone()).collect::<Vec<_>>(), vec![a]);
    assert!(app.views["other"].attachments.is_empty());
}

#[test]
fn generic_attachment_upload_uses_session_and_preserves_file_bytes() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let path = std::env::temp_dir().join(format!(
        "sill-attachment-{}.txt",
        sill_client::request_id("test")
    ));
    let bytes = b"notes\n\0binary-safe";
    std::fs::write(&path, bytes).unwrap();
    let task = app.attachment_paths("demo".into(), Some(vec![path.clone()]));
    let request = daemon.expect("_sill/uploads.create");
    let params: sill_proto::sfp::UploadCreate =
        serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(params.session_id, "demo");
    assert_eq!(
        sill_proto::admin::decode_bytes(&params.data_base64).unwrap(),
        bytes
    );
    daemon.reply(&request, json!({"upload_id":params.upload_id}));
    settle_task(&mut app, task);
    assert!(
        matches!(&demo_view(&app).attachments[0].attachment, sill_proto::sfp::Attachment::Uploaded { upload_id, .. } if upload_id == &params.upload_id)
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn markdown_image_is_read_from_session_host_and_decoded() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let source = "![result](/tmp/image%20one.png)";
    app.views.get_mut("demo").unwrap().rendered.insert(
        "test-image".into(),
        crate::Rendered {
            source: source.into(),
            markdown: iced::widget::markdown::Content::parse(source),
        },
    );
    let task = app.load_media("demo");
    let request = daemon.expect("_sill/files.image");
    let params: sill_proto::admin::SessionImage =
        serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(params.session_id, "demo");
    assert_eq!(
        params.path.to_path().unwrap(),
        std::path::PathBuf::from("/tmp/image one.png")
    );
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    daemon.reply(&request, json!({"bytes_base64":sill_proto::admin::encode_bytes(png.get_ref()),"next_offset":png.get_ref().len().to_string(),"eof":true,"revision":"one"}));
    settle_task(&mut app, task);
    assert!(
        demo_view(&app)
            .media
            .values()
            .any(|v| matches!(v, Some(Ok(_))))
    );
}

#[test]
fn new_tabs_append_and_refreshes_preserve_existing_positions() {
    let (mut app, _) = App::new(true, None);
    app.tabs.clear();
    let mut older = session_record("older", "Older", false, false);
    older.created_at = "9".into();
    let mut newer = session_record("newer", "Newer", false, false);
    newer.created_at = "10".into();
    let ids = |app: &App| {
        app.tabs
            .iter()
            .map(|t| t.id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let _ = app.update(Message::Listed(Ok(vec![newer.clone(), older.clone()])));
    assert_eq!(ids(&app), ["older", "newer"]);
    let mut newest = session_record("newest", "Newest", false, false);
    newest.created_at = "11".into();
    let _ = app.update(Message::Listed(Ok(vec![
        newest.clone(),
        newer.clone(),
        older.clone(),
    ])));
    assert_eq!(ids(&app), ["older", "newer", "newest"]);
    let _ = app.update(Message::Listed(Ok(vec![
        older.clone(),
        newest.clone(),
        newer.clone(),
    ])));
    assert_eq!(ids(&app), ["older", "newer", "newest"]);
    older.ended_at = Some("12".into());
    let _ = app.update(Message::Listed(Ok(vec![
        older.clone(),
        newest.clone(),
        newer.clone(),
    ])));
    older.ended_at = None;
    let _ = app.update(Message::Listed(Ok(vec![older, newest, newer])));
    assert_eq!(
        ids(&app),
        ["newer", "newest", "older"],
        "reopened tabs append too"
    );
}

#[test]
fn double_clicking_a_tab_title_starts_name_editing_without_dragging_window() {
    let (app, _) = App::new(true, None);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("witty-willow").unwrap();
    ui.click("witty-willow").unwrap();
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Name(crate::names::Event::Begin(id)) if id == "demo")),
        "{messages:?}"
    );
    assert!(
        !messages.iter().any(|m| matches!(m, Message::DragWindow)),
        "{messages:?}"
    );
}

#[test]
fn renaming_edits_the_session_name_and_summary_title_stays_independent() {
    use crate::names::Event;
    let (mut app, _) = App::new(true, None);
    let mut record = session_record("demo", "A descriptive summary title", false, false);
    record.name = Some("velvet-comet".into());
    app.all_sessions.push(record);
    let _ = app.name_event(Event::Begin("demo".into()));
    let editor = app.name_editor.as_ref().unwrap();
    assert_eq!(editor.text, "velvet-comet");
    let token = editor.token.clone();
    let _ = app.name_event(Event::Input("my-session".into()));
    let _ = app.name_event(Event::Done(
        token,
        "demo".into(),
        "my-session".into(),
        Ok(()),
    ));
    assert!(app.name_editor.is_none());
    assert_eq!(app.all_sessions[0].name.as_deref(), Some("my-session"));
    assert_eq!(app.all_sessions[0].title, "A descriptive summary title");
    assert_eq!(app.session_name("demo", "fallback"), "my-session");
    let _ = app.name_event(Event::Begin("demo".into()));
    let _ = app.name_event(Event::Input("not saved".into()));
    let _ = app.update(Message::Escape);
    assert!(app.name_editor.is_none());
    assert_eq!(app.session_name("demo", "fallback"), "my-session");
}

#[test]
fn slow_periodic_refreshes_do_not_accumulate_reads() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let session = session_record("demo", "demo", false, false);
    let (started, waiting) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let fake = std::thread::spawn(move || {
        // the list and the settings; the selected session's permission read is refused
        let mut requests = vec![];
        while requests.len() < 2 {
            let request = daemon.next();
            if request["method"] == "_sill/policy.preview" {
                daemon.refuse(&request, "unavailable", "not in this test");
            } else {
                requests.push(request);
            }
        }
        started.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        for request in requests {
            match request["method"].as_str().unwrap() {
                "_sill/sessions.list" => {
                    daemon.reply(&request, json!({"sessions": [session], "next": null}))
                }
                "_sill/settings.get" => {
                    daemon.reply(&request, sill_proto::admin::Settings::default())
                }
                other => panic!("unexpected refresh request {other}"),
            }
        }
    });
    let task = app.update(Message::Refresh);
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..20 {
        let duplicate = app.update(Message::Refresh);
        settle_task(&mut app, duplicate);
    }
    assert!(
        !app.list_again,
        "timer ticks must not perpetually invalidate slow reads"
    );
    release.send(()).unwrap();
    settle_task(&mut app, task);
    fake.join().unwrap();
    assert!(!app.list_pending && !app.settings_pending);
    assert_eq!(app.tabs.len(), 1);
    let _ = app.update(Message::Listed(Err("offline".into())));
    app.settings_pending = true;
    let _ = app.update(Message::DaemonSettings(
        app.settings_revision,
        Err("offline".into()),
    ));
    assert!(
        !app.list_pending && !app.settings_pending,
        "failed reads release their guards too"
    );
}

#[test]
fn explicit_relist_skips_obsolete_results_and_preserves_the_view() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    type_text(&mut app, "keep this draft");
    let session = session_record("demo", "reopened", false, false);
    let fake = std::thread::spawn(move || {
        let old = daemon.expect("_sill/sessions.list");
        daemon.reply(&old, json!({"sessions": [], "next": null}));
        let fresh = daemon.expect("_sill/sessions.list");
        daemon.reply(&fresh, json!({"sessions": [session], "next": null}));
    });
    let task = app.list();
    let _ = app.list();
    let _ = app.list();
    assert!(app.list_again);
    settle_task(&mut app, task);
    fake.join().unwrap();
    assert_eq!(demo_view(&app).draft.text(), "keep this draft");
    assert_eq!(app.tabs[0].title, "reopened");
    assert!(!app.list_pending && !app.list_again);
}

#[test]
fn failed_instructions_save_keeps_text_and_retry_enabled() {
    use crate::settings_ui::Event;
    let (mut app, _) = App::new(true, None);
    let task = app.show_page(crate::Page::Settings);
    settle_task(&mut app, task);
    let _ = app.settings_event(Event::Instructions(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(Arc::new(
            "new instructions".into(),
        ))),
    ));
    let before = app.settings_page.as_ref().unwrap().instructions.text();
    let task = app.settings_event(Event::SaveInstructions);
    assert!(
        app.settings_page.as_ref().unwrap().edited,
        "no success before the reply"
    );
    assert!(
        app.settings_page
            .as_ref()
            .unwrap()
            .instructions_saving
            .is_some()
    );
    settle_task(&mut app, task);
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!(page.instructions.text(), before);
    assert!(page.edited && page.instructions_saving.is_none());
    assert!(page.said.as_ref().unwrap().contains("Not changed"));
}

#[test]
fn settings_replies_cannot_overwrite_a_reopened_page() {
    use crate::settings_ui::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.show_page(crate::Page::Settings);
    let old = app.settings_page.as_ref().unwrap().token.clone();
    let _ = app.show_page(crate::Page::Settings);
    let new = app.settings_page.as_ref().unwrap().token.clone();
    assert_ne!(old, new);
    let stale = sill_client::settings::Snapshot {
        instructions: "old page's text".into(),
        ..Default::default()
    };
    let _ = app.update(Message::SettingsReply(
        old,
        Event::Loaded(String::new(), Box::new(Ok(stale))),
    ));
    assert!(app.settings_page.as_ref().unwrap().snapshot.is_none());
}

#[test]
fn instructions_edits_during_save_survive_older_snapshots() {
    use crate::settings_ui::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.show_page(crate::Page::Settings);
    let page = app.settings_page.as_mut().unwrap();
    page.instructions = iced::widget::text_editor::Content::with_text("newer text");
    page.edited = true;
    page.instructions_saving = Some("saved text".into());
    let _ = app.settings_event(Event::InstructionsSaved(
        "saved text".into(),
        sill_client::actions::Outcome::Done(json!({})),
    ));
    let _ = app.settings_event(Event::Loaded(
        app.settings_page.as_ref().unwrap().load_token.clone(),
        Box::new(Ok(sill_client::settings::Snapshot {
            instructions: "saved text".into(),
            ..Default::default()
        })),
    ));
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!(page.instructions.text(), "newer text");
    assert!(page.edited);
    let _ = app.settings_event(Event::Loaded(
        app.settings_page.as_ref().unwrap().load_token.clone(),
        Box::new(Ok(sill_client::settings::Snapshot {
            instructions: "newer text".into(),
            ..Default::default()
        })),
    ));
    assert!(!app.settings_page.as_ref().unwrap().edited);
}

#[test]
fn an_old_settings_load_cannot_replace_a_post_save_refresh() {
    use crate::settings_ui::Event;
    let (mut app, _) = App::new(true, None);
    let _ = app.show_page(crate::Page::Settings);
    let page = app.settings_page.as_ref().unwrap().token.clone();
    let old_load = app.settings_page.as_ref().unwrap().load_token.clone();
    let fresh = app.settings_load();
    settle_task(&mut app, fresh);
    let before = app.settings_page.as_ref().unwrap().instructions.text();
    let _ = app.update(Message::SettingsReply(
        page,
        Event::Loaded(
            old_load,
            Box::new(Ok(sill_client::settings::Snapshot {
                instructions: "stale server value".into(),
                ..Default::default()
            })),
        ),
    ));
    assert_eq!(
        app.settings_page.as_ref().unwrap().instructions.text(),
        before
    );
}

#[test]
fn background_settings_cannot_undo_a_newer_page_snapshot() {
    let (mut app, _) = App::new(true, None);
    let old_revision = app.settings_revision;
    let task = app.show_page(crate::Page::Settings);
    settle_task(&mut app, task);
    let before = app.daemon_settings.clone().unwrap();
    let _ = app.update(Message::DaemonSettings(
        old_revision,
        Ok(sill_proto::admin::Settings {
            theme: "stale-theme".into(),
            ..Default::default()
        }),
    ));
    assert_eq!(app.daemon_settings.as_ref().unwrap().theme, before.theme);
}

#[test]
fn prompt_undo_and_redo_shortcuts() {
    use iced::keyboard::{Key, Modifiers, key};
    use iced::widget::text_editor::{Binding, KeyPress, Status};
    for (character, modifiers, redo) in [
        ("z", Modifiers::COMMAND, false),
        ("z", Modifiers::COMMAND | Modifiers::SHIFT, true),
        ("y", Modifiers::COMMAND, true),
    ] {
        let press = KeyPress {
            key: Key::Character(character.into()),
            modified_key: Key::Character(character.into()),
            physical_key: key::Physical::Code(key::Code::KeyZ),
            modifiers,
            text: None,
            status: Status::Focused { is_hovered: false },
        };
        assert!(matches!(crate::ui::enter_sends(press.clone()),
            Some(Binding::Custom(Message::PromptHistory(value))) if value == redo));
        assert!(matches!(crate::ui::reading_keys(press),
            Some(Binding::Custom(Message::Files(crate::files::Action::History(value)))) if value == redo));
    }
}

/// The window's geometry through the app (`geometry.rs`): Sill's own placement is not taken for
/// the person's; the person's resize is kept for the display it is on; a display change puts the
/// window where it belongs without keeping what the window manager did meanwhile; Shelf mode
/// keeps only its width, for the main display.
#[test]
fn the_window_keeps_what_the_person_made_of_it_per_display() {
    use crate::geometry::{Controller, Display, Rect, target};
    use crate::layout::Mode;
    let rect = |x, y, width, height| Rect {
        x,
        y,
        width,
        height,
    };
    let laptop = Display {
        name: "eDP-1".into(),
        primary: true,
        bounds: rect(0, 0, 1920, 1080),
        work: rect(0, 0, 1920, 1040),
    };
    let external = Display {
        name: "DP-1".into(),
        primary: false,
        bounds: rect(1920, 0, 2560, 1440),
        work: rect(1920, 0, 2560, 1440),
    };
    let both = vec![laptop.clone(), external];
    let poll = |app: &mut App, displays: &[Display], window: Rect| {
        let read = Some((displays.to_vec(), Some(window)));
        let _ = app.update(Message::Geometry(crate::Read::Poll, read));
    };
    let (mut app, _) = App::new(true, None);
    app.geometry = Controller::immediate();
    app.visible = true;
    app.layout.mode = Mode::Drawer;
    // placed by Sill: half the main display, its usable height
    let placed = target(Mode::Drawer, &app.settings, &both).unwrap();
    assert_eq!(placed.rect, rect(0, 0, 960, 1040));
    let _ = app.update(Message::Placed(Some((both.clone(), Some(placed.clone())))));
    poll(&mut app, &both, placed.rect);
    poll(&mut app, &both, placed.rect);
    assert!(app.settings.displays.is_empty(), "Sill's own placement is not kept");
    // the person drags it onto the external display and sizes it there
    let dragged = rect(2560, 360, 1280, 720);
    poll(&mut app, &both, dragged);
    poll(&mut app, &both, dragged);
    let kept = &app.settings.displays["DP-1"];
    assert_eq!(
        (kept.work_width, kept.work_height, kept.work_x, kept.work_y),
        (Some(0.5), Some(0.5), Some(0.25), Some(0.25))
    );
    assert_eq!(app.settings.display.as_deref(), Some("DP-1"));
    // the external display unplugged: the window manager's move is not kept, and the external
    // display's preferences stay for when it is back
    poll(&mut app, &[laptop], rect(0, 0, 1280, 720));
    assert_eq!(app.settings.display.as_deref(), Some("DP-1"));
    assert!(!app.settings.displays.contains_key("eDP-1"));
    // Shelf mode keeps only its width, for the main display
    app.layout.mode = Mode::Shelf;
    let shelf = target(Mode::Shelf, &app.settings, &both).unwrap();
    assert_eq!(shelf.rect, rect(0, 0, 672, 1040));
    let _ = app.update(Message::Placed(Some((both.clone(), Some(shelf.clone())))));
    poll(&mut app, &both, shelf.rect);
    let narrower = Rect {
        width: 576,
        ..shelf.rect
    };
    poll(&mut app, &both, narrower);
    poll(&mut app, &both, narrower);
    assert_eq!(app.settings.displays["eDP-1"].shelf_width, Some(0.3));
    assert_eq!(app.settings.displays["eDP-1"].work_width, None, "no working size");
    assert_eq!(app.shelf_share(), 0.3, "Settings shows the main display's");
}

/// Ctrl+Shift+D pressed again before the first change's window read landed: one read, one
/// placement at a time, and the window ends up placed for the mode it is in. Two reads in flight
/// at once kept the Shelf's geometry as the Drawer's (2026-11-08).
#[test]
fn mode_changes_in_flight_are_serialized() {
    use crate::geometry::{target, Controller, Display, Rect};
    use crate::layout::{Action, Mode};
    let display = Display {
        name: "eDP-1".into(),
        primary: true,
        bounds: Rect { x: 0, y: 0, width: 1920, height: 1080 },
        work: Rect { x: 0, y: 0, width: 1920, height: 1040 },
    };
    let displays = vec![display];
    let (mut app, _) = App::new(true, None);
    app.geometry = Controller::immediate();
    app.visible = true;
    app.xid = Some(1);
    app.layout.mode = Mode::Drawer;
    let drawer = target(Mode::Drawer, &app.settings, &displays).unwrap();
    let _ = app.update(Message::Placed(Some((displays.clone(), Some(drawer.clone())))));
    // into Shelf mode: the window is read first
    let _ = app.update(Message::Layout(Action::ToggleShelf));
    assert_eq!(app.layout.mode, Mode::Shelf);
    assert_eq!(app.switching, Some(Mode::Shelf));
    // and back before the read landed: the mode changes, nothing else starts
    let _ = app.update(Message::Layout(Action::ToggleShelf));
    assert_eq!(app.layout.mode, Mode::Drawer);
    assert_eq!(app.switching, Some(Mode::Shelf), "the change under way is the one that lands");
    // the read lands with the window as the Drawer had it: nothing of it is the Shelf's
    let read = Some((displays.clone(), Some(drawer.rect)));
    let _ = app.update(Message::Geometry(crate::Read::Leave(Mode::Drawer), read));
    assert!(app.settings.displays.is_empty(), "Sill's own placement is not kept");
    assert_eq!(app.switching, Some(Mode::Drawer), "placed for the mode it is in now");
    // placed for Drawer, as it is: the change is over
    let _ = app.update(Message::SwitchPlaced(Some((displays.clone(), Some(drawer.clone())))));
    assert_eq!(app.switching, None);
    // placed for a mode it has since left: placed again
    let _ = app.update(Message::Layout(Action::ToggleShelf));
    assert_eq!(app.switching, Some(Mode::Shelf));
    let shelf = target(Mode::Shelf, &app.settings, &displays).unwrap();
    let _ = app.update(Message::Geometry(crate::Read::Leave(Mode::Drawer), Some((displays.clone(), Some(drawer.rect)))));
    let _ = app.update(Message::Layout(Action::ToggleShelf));
    assert_eq!(app.layout.mode, Mode::Drawer);
    let _ = app.update(Message::SwitchPlaced(Some((displays.clone(), Some(shelf)))));
    assert_eq!(app.switching, Some(Mode::Drawer), "placed again for Drawer");
    let _ = app.update(Message::SwitchPlaced(Some((displays, Some(drawer)))));
    assert_eq!(app.switching, None);
}

#[test]
fn markdown_link_click_navigates_and_secondary_click_copies() {
    let (mut app, _) = App::new(true, None);
    let source = sill_proto::sessions::HostPath::from_path(std::path::Path::new(
        "/home/demo/project/README.md",
    ));
    let _ = app.update(Message::Files(crate::files::Action::Open {
        path: source.clone(),
        repo: None,
    }));
    let view = app.views.get_mut("demo").unwrap();
    let file = view.files.reader.as_mut().unwrap();
    file.text = "A [document link](docs/next.md#heading).".into();
    file.revision += 1;
    view.sync_reader();
    let mut ui = simulator(&app, (1000.0, 860.0));
    let at = aim_at(&mut ui, "A document link", "document");
    ui.aim(at);
    ui.send(vec![iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
        iced::mouse::Button::Right,
    ))]);
    assert_eq!(copies(ui.into_messages()), ["docs/next.md#heading"]);
    let mut ui = simulator(&app, (1000.0, 860.0));
    let at = aim_at(&mut ui, "A document link", "document");
    ui.aim(at);
    ui.send(vec![
        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        )),
    ]);
    assert!(ui.into_messages().any(|m| matches!(m, Message::Files(crate::files::Action::Link { session, source: from, target })
        if session == "demo" && from.display == source.display && target == "docs/next.md#heading")));
}

#[test]
fn closing_tabs_returns_to_the_most_recent_open_tab() {
    let (mut app, _) = App::new(true, None);
    let records = || vec![
        session_record("demo", "Demo", false, false),
        session_record("middle", "Middle", false, false),
        session_record("last", "Last", false, false),
    ];
    let _ = app.update(Message::Listed(Ok(records())));
    let _ = app.choose("last".into());
    let _ = app.choose("middle".into());
    // Repeated activation and refreshes must not change the return order.
    let _ = app.choose("middle".into());
    let _ = app.update(Message::Listed(Ok(records())));
    let mut remaining = records();
    remaining.retain(|s| s.session_id != "middle");
    let _ = app.update(Message::Listed(Ok(remaining.clone())));
    assert_eq!(app.selected.as_deref(), Some("last"));
    remaining.retain(|s| s.session_id != "last");
    let _ = app.update(Message::Listed(Ok(remaining)));
    assert_eq!(app.selected.as_deref(), Some("demo"));
    let _ = app.update(Message::Listed(Ok(vec![])));
    assert!(app.selected.is_none());
    assert!(app.tab_history.is_empty());
}

#[test]
fn closing_a_background_tab_preserves_selection_and_skips_it_in_history() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "middle");
    add_session(&mut app, "last");
    let _ = app.choose("middle".into());
    let _ = app.choose("last".into());
    let demo = session_record("demo", "Demo", false, false);
    let last = session_record("last", "Last", false, false);
    let _ = app.update(Message::Listed(Ok(vec![demo.clone(), last])));
    assert_eq!(app.selected.as_deref(), Some("last"));
    let _ = app.update(Message::Listed(Ok(vec![demo])));
    assert_eq!(app.selected.as_deref(), Some("demo"));
}

/// The session's state with `summarizing` set or cleared, as its lane delivers it.
fn summary_update(app: &App, id: &str, kind: Option<&str>) -> Message {
    let view = &app.views[id];
    let mut session = view.conversation.session.clone().unwrap();
    session.summarizing = kind.map(str::to_owned);
    session.activity = if kind.is_some() { "working" } else { "idle" }.into();
    let event = SessionEvent {
        session_id: id.into(),
        subscription_id: view.conversation.subscription_id.clone(),
        seq: (view.conversation.seq + 1).to_string(),
        event: Event::SessionUpdated { session },
    };
    Message::Lane(
        id.into(),
        view.lane_epoch,
        sill_client::lane::Update::Event(Box::new(event)),
    )
}

#[test]
fn a_summary_beginning_returns_to_the_session_before_and_folds_the_tab() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "first");
    add_session(&mut app, "second");
    add_session(&mut app, "third");
    let _ = app.choose("first".into());
    let _ = app.choose("third".into());
    let _ = app.choose("second".into());
    // the session visited before is summarizing too: it is skipped
    let _ = app.update(summary_update(&app, "third", Some("checkpoint")));
    assert_eq!(app.selected.as_deref(), Some("second"), "a background summary");
    let _ = app.update(summary_update(&app, "second", Some("checkpoint")));
    assert!(app.views["second"].summarizing());
    assert_eq!(app.selected.as_deref(), Some("first"));
    // the folded tabs lead the bar, in their own order; numbers and steps follow the bar
    let order = |app: &App| app.shown_tabs().iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    let shown = order(&app);
    assert_eq!(shown[..2], ["second", "third"]);
    let _ = app.update(Message::SelectTab(0));
    assert_eq!(app.selected.as_deref(), Some("second"));
    let _ = app.update(Message::StepTab(1));
    assert_eq!(app.selected.as_deref(), Some("third"));
    let _ = app.choose("first".into());
    // opening it again while it summarizes keeps it open: only the beginning moves on
    let _ = app.choose("second".into());
    let _ = app.update(summary_update(&app, "second", Some("end")));
    assert_eq!(app.selected.as_deref(), Some("second"));
    let _ = app.update(summary_update(&app, "second", None));
    assert!(!app.views["second"].summarizing());
    // its summary over, the tab is back in its place
    let shown = order(&app);
    assert_eq!(shown[0], "third");
    assert!(shown.iter().position(|id| id == "first") < shown.iter().position(|id| id == "second"));
    // the folded tab is only its glyph: its title is not drawn, the others' are
    let mut ui = simulator(&app, (720.0, 200.0));
    assert!(ui.find("second-title").is_ok());
    assert!(ui.find("third-title").is_err());
    assert!(ui.find("first-title").is_ok());
}

#[test]
fn the_only_session_stays_selected_when_it_summarizes() {
    let (mut app, _) = App::new(true, None);
    app.tabs.clear();
    app.views.clear();
    app.tab_history.clear();
    add_session(&mut app, "only");
    let _ = app.choose("only".into());
    let _ = app.update(summary_update(&app, "only", Some("checkpoint")));
    assert_eq!(app.selected.as_deref(), Some("only"));
}

#[test]
fn a_session_that_ends_loses_its_tab_before_the_list_returns() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "first");
    add_session(&mut app, "second");
    let _ = app.choose("first".into());
    let _ = app.choose("second".into());
    let _ = app.update(summary_update(&app, "second", Some("end")));
    assert_eq!(app.selected.as_deref(), Some("first"));
    assert!(simulator(&app, (720.0, 200.0)).find("second-title").is_err(), "folded while summarizing");
    // the daemon's end: lifecycle ended and summarizing null in one update, no list yet
    let ended = |app: &App, id: &str| {
        let view = &app.views[id];
        let mut session = view.conversation.session.clone().unwrap();
        session.summarizing = None;
        session.lifecycle = "ended".into();
        session.activity = "idle".into();
        let event = SessionEvent {
            session_id: id.into(),
            subscription_id: view.conversation.subscription_id.clone(),
            seq: (view.conversation.seq + 1).to_string(),
            event: Event::SessionUpdated { session },
        };
        Message::Lane(id.into(), view.lane_epoch, sill_client::lane::Update::Event(Box::new(event)))
    };
    let _ = app.update(ended(&app, "second"));
    assert!(!app.tabs.iter().any(|t| t.id == "second"), "the tab went with the end");
    assert!(!app.views.contains_key("second"));
    assert!(!app.tab_history.iter().any(|id| id == "second"));
    assert!(simulator(&app, (720.0, 200.0)).find("second-title").is_err());
    assert_eq!(app.selected.as_deref(), Some("first"));
    // the selected session ending: the person lands on the tab visited before it
    let _ = app.choose("demo".into());
    let _ = app.choose("first".into());
    let _ = app.update(ended(&app, "first"));
    assert_eq!(app.selected.as_deref(), Some("demo"));
}

#[test]
fn acp_controls_distinguish_loading_restoring_empty_and_failed() {
    let mut app = streamed();
    let view = app.views.get_mut("demo").unwrap();
    view.status = sill_client::lane::Status::Live;
    view.conversation.session.as_mut().unwrap().controls = None;
    simulator(&app, (1200.0, 860.0)).find("Loading controls…").unwrap();
    let controls = SessionControls { status: "restoring".into(), error: None,
        current_mode: Some("saved".into()), modes: vec![ControlChoice { value: "saved".into(), title: "Saved selection".into() }], options: vec![] };
    app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls = Some(controls);
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        ui.find("Refreshing controls…").unwrap();
        ui.find("Saved selection").unwrap();
        ui.click("Saved selection").unwrap();
        assert!(!ui.into_messages().any(|message| matches!(message, Message::SetMode(_) | Message::SetOption(..))));
    }
    app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap().status = "ready".into();
    app.views.get_mut("demo").unwrap().status = sill_client::lane::Status::Reconnecting("offline".into());
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        ui.find("Refreshing controls…").unwrap();
        ui.click("Saved selection").unwrap();
        assert!(!ui.into_messages().any(|message| matches!(message, Message::SetMode(_))));
    }
    app.views.get_mut("demo").unwrap().status = sill_client::lane::Status::Live;
    let controls = app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap();
    controls.status = "ready".into(); controls.current_mode = None; controls.modes.clear();
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        assert!(ui.find("Loading controls…").is_err());
        assert!(ui.find("Refreshing controls…").is_err());
    }
    let controls = app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap();
    controls.status = "failed".into(); controls.error = Some("Agent did not confirm effort".into());
    assert!(simulator(&app, (1200.0, 860.0)).find("Settings warning").is_err());
}

/// The modifier that moves by words, as iced's editors take it (`Modifiers::jump`): Alt (Option)
/// on macOS, Ctrl elsewhere.
const JUMP: iced::keyboard::Modifiers = if cfg!(target_os = "macos") {
    iced::keyboard::Modifiers::ALT
} else {
    iced::keyboard::Modifiers::CTRL
};

/// A model's raw reasoning is folded to one line that opens to it; a short thought (Codex's
/// summary titles) is shown as it is.
#[test]
fn a_long_thought_folds_to_its_last_line() {
    use crate::ui::{is_long_thought, thought_line};
    assert!(!is_long_thought("\n\n**Adding UI snapshot test**\n\n**Reviewing UI behavior**"));
    let raw = "Let me look at the feed.\n\nHmm, wait. Actually the heights are measured.\n\nOK so the row is measured.\n\nSo I will fold the `thought` to *one* line.\n";
    assert!(is_long_thought(raw));
    assert_eq!(thought_line(raw), "So I will fold the thought to one line.");
    // Codex's summaries: the latest title, not the prose under it
    assert_eq!(thought_line("**Checking icons**\n\nprose\n\n**Adding a test**\n\nmore prose"), "Adding a test");

    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let view = app.views.get_mut("demo").unwrap();
    view.conversation.questions.clear();
    view.conversation.insert_item(Item {
        item_id: "raw".into(),
        created_seq: "900".into(),
        turn_id: Some("demo-turn".into()),
        submission_id: None,
        agent_generation_id: "demo-generation".into(),
        kind: "thought".into(),
        content: vec![Content::text(raw)],
        tool: None,
    });
    view.render("raw");
    view.follow_tail = true;
    let mut ui = simulator(&app, (1200.0, 860.0));
    ui.click("So I will fold the thought to one line.").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(messages.as_slice(), [Message::ToggleTool(id)] if id == "raw"));
}
