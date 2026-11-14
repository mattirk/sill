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

    pub fn expect(&mut self, method: &str) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let request: Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("no request; expected {method}"));
        assert_eq!(request["method"], method, "{request}");
        request
    }

    pub fn reply(&mut self, request: &Value, result: impl serde::Serialize) {
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
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
fn live_app(clients: Vec<Client>) -> App {
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
fn settle_task(app: &mut App, task: Task<Message>) {
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

fn event(view: &sill_client::Conversation, payload: Event) -> SessionEvent {
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

fn simulator(app: &App, size: (f32, f32)) -> iced_test::Simulator<'_, Message> {
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

#[test]
fn transcript_selection_is_read_only_and_stable_during_streaming() {
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::ToggleText);
    let original = demo_view(&app).text_view.as_ref().unwrap().text();
    let _ = app.update(Message::TextAction(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Insert('X')),
    ));
    let _ = app.update(Message::DemoTick);
    assert_eq!(demo_view(&app).text_view.as_ref().unwrap().text(), original);
    let _ = app.update(Message::RefreshText);
    let text = demo_view(&app).text_view.as_ref().unwrap().text();
    assert!(text.contains("A streamed paragraph"));
    assert!(!text.starts_with('X'));
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

/// Stop removes prompts still queued first, as the shelf does, then stops the turn.
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
    let (second, mut lost) = self::daemon();
    let (third, mut checker) = self::daemon();
    let mut app = live_app(vec![first, second, third]);

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
    drop(daemon);
    let task = app.update(Message::Send);
    let request = lost.expect("_sill/sessions.submit");
    drop(lost);
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

/// With a file covering the conversation, the strip keeps the session's question — answerable —
/// and Stop in reach, and the way back.
#[test]
fn the_status_strip_keeps_the_question_and_stop_in_reach_while_a_file_is_open() {
    use crate::workspace::Action;
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let _ = app.update(Message::Workspace(Action::ToggleDrawer));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("README.md").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(!demo_view(&app).conversation_shown());
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.find("Waiting for your answer: Edit conversation.rs")
        .unwrap();
    ui.click("Yes").unwrap();
    ui.click(iced::widget::Id::new("stop")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::Answer { option_id, .. }, Message::Stop] if option_id == "allow"),
        "{messages:?}"
    );
    // the transcript covers it too; from there the strip has the way back
    let _ = app.update(Message::Workspace(Action::CloseViewer));
    let _ = app.update(Message::ToggleText);
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("‹ Conversation").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).conversation_shown());
}

/// Another session waiting on the person shows in its tab and, while files cover the
/// conversation, in the strip; choosing it goes there.
#[test]
fn other_sessions_waiting_on_the_person_are_marked() {
    use crate::workspace::Action;
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
    let _ = app.update(Message::Workspace(Action::Open {
        path: sill_proto::sessions::HostPath::from_path(std::path::Path::new(
            "/home/demo/project/README.md",
        )),
        repo: None,
    }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("? other-title").unwrap();
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
    let (app, _) = App::new(true, None);
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
        crate::keys::shortcut(press(Modifiers::CTRL)),
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

/// `>_` opens the shell in the prompt's place; typed keys go to it, not to the prompt.
#[test]
fn the_terminal_button_opens_the_shell_and_takes_the_keys() {
    let (mut app, _) = App::new(true, None);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(">_").unwrap();
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
    use crate::workspace::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Workspace(Action::ToggleDrawer));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("README.md").unwrap();
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Workspace(Action::Open { .. })))
    );
    for message in messages {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).files.reader.is_some());
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("‹ Conversation").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(demo_view(&app).files.reader.is_none());
    assert!(!demo_view(&app).files.open_files.is_empty());
}

#[test]
fn diff_numbers_distinguish_context_changes_and_untracked_markdown() {
    use crate::workspace_ui::numbered_diff;
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
    use crate::workspace::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Workspace(Action::ToggleDrawer));
    let mut ui = simulator(&app, (480.0, 700.0));
    ui.click("README.md").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert!(!app.workspace.shown);
    assert!(demo_view(&app).files.reader.is_some());
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
        iced::Event::Keyboard(Event::ModifiersChanged(Modifiers::CTRL)),
        iced::Event::Keyboard(Event::KeyPressed {
            key: Key::Character("c".into()),
            modified_key: Key::Character("c".into()),
            physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::CTRL,
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
    let mut live = Live::new((1000.0, 400.0));
    live.apply(&mut app, Message::Latest);
    let mut before = None;
    live.frame(&app, |ui| before = scrolled(ui, "conversation"));
    live.apply(
        &mut app,
        Message::Workspace(crate::workspace::Action::ToggleDrawer),
    );
    assert!(app.workspace.shown);
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
    ui.click("rg -n 'sill-send' crates/sill/src/shelf").unwrap();
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
    use crate::workspace::Action;
    use sill_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_path(std::path::Path::new(s));
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Workspace(Action::Open {
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
    let _ = app.update(Message::Workspace(Action::DiffMode(true)));
    let _ = app.update(Message::Workspace(Action::Open {
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
            Some(Message::Workspace(action)) => {
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
    assert_eq!(names[1], "choose demo", "{names:?}");
    assert_eq!(names[2], "Menu(Toggle)", "{names:?}");
    assert_eq!(names[3], "ToggleText", "{names:?}");
    assert!(names[4].starts_with("files ToggleDrawer"), "{names:?}");
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
    let spaced = at_each_stop(&mut app, (720.0, 860.0), 2, Named::Space);
    assert!(
        matches!(&spaced[1], Some(Message::Choose(id)) if id == "demo"),
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

/// The shelf's shortcuts (`shelf/window.rs`), and how the prompt passes them on.
#[test]
fn shortcuts_match_the_shelf_and_work_from_the_prompt() {
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
    let both = Modifiers::CTRL | Modifiers::SHIFT;
    assert!(matches!(
        shortcut(press(named(Named::Tab), Modifiers::empty())),
        Some(Message::Focus(true))
    ));
    assert!(matches!(
        shortcut(press(named(Named::Tab), Modifiers::SHIFT)),
        Some(Message::Focus(false))
    ));
    assert!(matches!(
        shortcut(press(named(Named::ArrowRight), both)),
        Some(Message::StepTab(1))
    ));
    assert!(matches!(
        shortcut(press(named(Named::ArrowLeft), both)),
        Some(Message::StepTab(-1))
    ));
    assert!(matches!(
        shortcut(press(ch("F"), both)),
        Some(Message::Workspace(crate::workspace::Action::ToggleDrawer))
    ));
    assert!(matches!(
        shortcut(press(ch("t"), Modifiers::CTRL)),
        Some(Message::ToggleTerminal)
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
        shortcut(press(named(Named::End), Modifiers::CTRL)),
        Some(Message::Latest)
    ));
    assert!(matches!(
        shortcut(press(ch("q"), Modifiers::CTRL)),
        Some(Message::Quit)
    ));
    assert!(shortcut(press(ch("a"), Modifiers::empty())).is_none());

    // in the prompt: tab switching beats word selection, PageUp pages the conversation, and
    // Tab is left to the window
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
        crate::ui::enter_sends(typed(named(Named::ArrowLeft), both)),
        Some(Binding::Custom(Message::StepTab(-1)))
    ));
    assert!(matches!(
        crate::ui::enter_sends(typed(named(Named::PageUp), Modifiers::empty())),
        Some(Binding::Custom(Message::Page(false)))
    ));
    assert!(crate::ui::enter_sends(typed(named(Named::Tab), Modifiers::empty())).is_none());
}

/// Ctrl+Shift+Left/Right walk the tabs, round the ends.
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

/// Escape steps back as in the shelf: the open file, then the transcript, then the drawer.
#[test]
fn escape_steps_back() {
    use crate::workspace::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Workspace(Action::ToggleDrawer));
    let _ = app.update(Message::Workspace(Action::Open {
        path: sill_proto::sessions::HostPath::from_path(std::path::Path::new(
            "/home/demo/project/README.md",
        )),
        repo: None,
    }));
    let _ = app.update(Message::ToggleText);
    assert!(demo_view(&app).files.reader.is_some() && demo_view(&app).text_view.is_some());
    let _ = app.update(Message::Escape);
    assert!(demo_view(&app).files.reader.is_none(), "the file first");
    assert!(demo_view(&app).text_view.is_some());
    let _ = app.update(Message::Escape);
    assert!(demo_view(&app).text_view.is_none(), "then the transcript");
    assert!(app.workspace.shown);
    let _ = app.update(Message::Escape);
    assert!(!app.workspace.shown, "then the drawer");
}

// --- sessions, forms, held prompts ---------------------------------------------------------------

fn session_record(
    id: &str,
    title: &str,
    ended: bool,
    ghost: bool,
) -> sill_proto::sessions::Session {
    serde_json::from_value(json!({
        "session_id": id, "agent": "claude", "title": title, "title_kind": "",
        "created_at": "1", "last_used": "1", "ended_at": ended.then_some("1793806135"),
        "policy": "default", "auto_allow": false, "desktop": "", "memory_scope": "",
        "summary": if ended { "made the arrow smaller" } else { "" }, "summary_kind": "",
        "ghost": ghost, "running": !ended, "pins": [], "settings": {}, "follows": null,
        "compacted": "0"
    }))
    .unwrap()
}

/// The menu shows the agents with their ghost buttons, then the sessions to reopen — the first
/// five and "N more…" — and its rows ask for what they say.
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
    ui.find("Reopen").unwrap();
    ui.find("2 more…").unwrap();
    assert!(ui.find("Ended session 6").is_err(), "only the first five");
    ui.click("Ended session 1").unwrap();
    ui.click("Codex").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::Menu(Event::Reopen(id)), Message::Menu(Event::New { agent, ghost: false })] if id == "old1" && agent == "codex"),
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
    ui.click("×").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(&messages[..], [Message::CloseTab(id)] if id == "demo"),
        "{messages:?}"
    );
    let _ = app.update(Message::CloseTab("demo".into()));
    assert_eq!(app.armed_close.as_deref(), Some("demo"));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Close?").unwrap();
    drop(ui);
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

/// A form question is answered in place: its fields, then Submit with the values as the shelf
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
    let page = app.changes.as_ref().unwrap();
    let entries = page.entries.as_ref().unwrap().as_ref().unwrap();
    entries
        .iter()
        .find(|e| sill_client::changes::path(e).display.ends_with(file))
        .map(sill_client::changes::entry_key)
        .unwrap()
}

/// The herring opens the Changes page: the pinned repository's live changes, the session's writes
/// (one changed on the host, which cannot be kept) and an earlier keep to undo. A row opens its
/// diff; reverting takes a second press; Escape goes back.
#[test]
fn the_changes_page_groups_what_sessions_wrote() {
    use crate::changes_ui::Event;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let mut ui = simulator(&app, (720.0, 860.0));
    for shown in [
        "5 pending change(s)",
        "/home/demo/project",
        "live",
        "witty-willow",
        "no git",
        "Keep all",
        "/home/demo/.config/app/settings.toml",
        "changed on the host too",
        "Kept from witty-willow",
        "Undo",
    ] {
        ui.find(shown)
            .unwrap_or_else(|_| panic!("{shown} is not shown"));
    }
    ui.click("/home/demo/.config/app/settings.toml").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    for message in messages {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("+theme = \"dracula\"").unwrap();
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

/// The page's actions are the daemon's requests: keep with the version shown, revert all on a
/// second press, git's revert for a live file, undo for a keep; the page reads again after each.
#[test]
fn changes_actions_are_the_public_requests() {
    use crate::changes_ui::{ChangesPage, Event};
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.changes = Some(ChangesPage {
        entries: Some(Ok(demo::changes())),
        ..ChangesPage::default()
    });
    let keep = changes_key(&app, "settings.toml");
    let live = changes_key(&app, "README.md");
    let kept = changes_key(&app, ".bashrc");
    let server = std::thread::spawn(move || {
        let reload = |daemon: &mut Daemon| {
            for _ in 0..2 {
                let request = daemon.next();
                match request["method"].as_str() {
                    Some("_sill/changes.list") => {
                        daemon.reply(&request, json!({"entries": [], "next": null}))
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
    assert_eq!(page.auto_revert, 1440);
    // the page read again (empty now); the rest act on the demo's entries
    app.changes.as_mut().unwrap().entries = Some(Ok(demo::changes()));
    let all: Vec<String> = ["settings.toml", "plan.md", ".profile"]
        .iter()
        .map(|f| changes_key(&app, f))
        .collect();
    for _ in 0..2 {
        let task = app.update(Message::Changes(Event::RevertAll(all.clone())));
        settle_task(&mut app, task);
    }
    app.changes.as_mut().unwrap().entries = Some(Ok(demo::changes()));
    for _ in 0..2 {
        let task = app.update(Message::Changes(Event::Revert(live.clone())));
        settle_task(&mut app, task);
    }
    app.changes.as_mut().unwrap().entries = Some(Ok(demo::changes()));
    let task = app.update(Message::Changes(Event::Undo(kept)));
    settle_task(&mut app, task);
    server.join().unwrap();
}
