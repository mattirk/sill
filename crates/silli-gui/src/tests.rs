use silli_proto::method;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::Task;
use serde_json::{Value, json};
use silli_client::actions::Actions;
use silli_client::conversation::item_text;
use silli_client::outbox::State;
use silli_proto::{Client, sfp::*};

use crate::{App, Message, SessionView, Tab, demo, session};

// --- harness ------------------------------------------------------------------------------------

/// `event` as the selected session's view sends it.
pub fn in_session(app: &App, event: session::Event) -> Message {
    session::message(app.selected.as_deref().expect("a selected session"), event)
}

/// The daemon's end of a socket pair: read the app's requests, answer or refuse them.
pub struct Daemon {
    reader: BufReader<Socket>,
    writer: Socket,
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
            let request: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("no request; expected {method}"));
            // reads that go out beside the one under test (the panel reads Host control's state
            // with its policy views, concurrently; a session's workspaces are read with its files
            // and the Changes page)
            let incidental = [method::POLICY_PREVIEW, method::HOST_CONTROL_STATE, method::WORKSPACES_SESSION, method::WORKSPACES_LIST];
            if incidental.contains(&request["method"].as_str().unwrap_or("")) && request["method"] != method {
                self.refuse(&request, "unavailable", "not in this test");
                continue;
            }
            assert_eq!(request["method"], method, "{request}");
            return request;
        }
    }

    pub fn reply(&mut self, request: &Value, result: impl serde::Serialize) {
        writeln!(self.writer, "{}", json!({"jsonrpc":"2.0","id":request["id"],"result":result})).unwrap();
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

/// The two ends of a connection inside this process: a Unix socket pair where there is one, a
/// loopback connection on Windows.
#[cfg(unix)]
type Socket = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Socket = std::net::TcpStream;

fn socket_pair() -> (Socket, Socket) {
    #[cfg(unix)]
    return Socket::pair().unwrap();
    #[cfg(windows)]
    {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let ours = Socket::connect(listener.local_addr().unwrap()).unwrap();
        (ours, listener.accept().unwrap().0)
    }
}

/// A client of the protocol over one end.
fn client_over(socket: Socket) -> Client {
    let (reader, close) = (socket.try_clone().unwrap(), socket.try_clone().unwrap());
    Client::over_io(reader, socket, move || {
        let _ = close.shutdown(std::net::Shutdown::Both);
    })
    .unwrap()
}

pub fn daemon() -> (Client, Daemon) {
    let (socket, peer) = socket_pair();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let reader = BufReader::new(peer.try_clone().unwrap());
    (client_over(socket), Daemon { reader, writer: peer })
}

/// A connector handing out these clients one connection at a time, then refusing.
pub fn connector(clients: Vec<Client>) -> silli_client::Connector {
    let clients = Arc::new(Mutex::new(clients.into_iter().map(Some).collect::<Vec<_>>()));
    Arc::new(move || {
        let mut all = clients.lock().unwrap();
        match all.iter_mut().find_map(Option::take) {
            Some(client) => Ok(client),
            None => anyhow::bail!("silli daemon at /fake: connection refused"),
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
        assert!(std::time::Instant::now() < deadline, "a task never finished");
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
    let _ = app.update(in_session(app, session::Event::Prompt(Action::Edit(Edit::Paste(Arc::new(text.to_string()))))));
}

fn demo_view(app: &App) -> &SessionView {
    &app.views["demo"]
}

pub(crate) fn event(view: &silli_client::Conversation, payload: Event) -> SessionEvent {
    SessionEvent { session_id: view.session_id.clone(), subscription_id: view.subscription_id.clone(), seq: (view.seq + 1).to_string(), event: payload }
}

/// A second open session beside the demo's, with a conversation of its own.
fn add_session(app: &mut App, id: &str) {
    app.tabs.push(Tab { id: id.into(), title: format!("{id}-title"), agent: "codex".into(), running: true });
    let mut view = SessionView::new(id);
    view.conversation = silli_client::Conversation::new(id, "sub", 0);
    view.conversation.session = demo::conversation().session.map(|mut s| {
        s.session_id = id.into();
        s.activity = "idle".into();
        s.turn_id = None;
        s
    });
    view.status = silli_client::lane::Status::Live;
    app.views.insert(id.into(), view);
}

/// The renderer every test draws with: the software one. A GPU renderer makes a Vulkan device per
/// test, and devices made from several test threads at once crash in the system's Vulkan loader
/// (a segmentation fault in about one parallel run in thirty, 2026-11-17). `iced_test`'s own
/// simulator takes the same choice from `ICED_TEST_BACKEND`, set in `.cargo/config.toml`.
pub(crate) const TEST_BACKEND: Option<&str> = Some("tiny-skia");

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
    assert!(view.accept(demo::append(5, " contiguous")).unwrap().is_some());
}

#[test]
fn older_item_appends_stay_in_place_and_unknown_blocks_have_text_fallback() {
    let mut view = demo::conversation();
    let mut later = view.item("agent").unwrap().clone();
    later.item_id = "later".into();
    later.created_seq = "10".into();
    later.content = vec![Content { kind: "future_extension".into(), ..Content::text("fallback") }];
    view.accept(event(&view, Event::ItemCreated { item: later })).unwrap();
    view.accept(demo::append(5, "arrived later")).unwrap();
    let ids: Vec<_> = view.visible_items().into_iter().map(|i| i.item_id.as_str()).collect();
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
    view.accept(event(&view, Event::SubmissionUpdated { submission: submission.clone() })).unwrap();
    assert!(!view.visible_items().iter().any(|i| i.kind == "user"));
    submission.delivery = "sent".into();
    view.accept(event(&view, Event::SubmissionUpdated { submission })).unwrap();
    assert!(view.visible_items().iter().any(|i| i.kind == "user"));
    for event in demo::waiting(view.seq + 1) {
        view.accept(event).unwrap();
    }
    assert!(view.open_questions().next().is_some());
    let mut question = view.questions["demo-question"].clone();
    question.state = "answered".into();
    view.accept(event(&view, Event::QuestionUpdated { question })).unwrap();
    assert!(view.open_questions().next().is_none());
}

fn read_request(reader: &mut BufReader<Socket>, method: &str) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], method);
    request
}

fn reply(writer: &mut Socket, request: &Value, result: impl serde::Serialize) {
    writeln!(writer, "{}", json!({"jsonrpc":"2.0","id":request["id"],"result":result})).unwrap();
}

#[test]
fn fragmented_snapshot_and_live_handoff_over_real_protocol_transport() {
    let (socket, mut peer) = socket_pair();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let source = demo::conversation();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        let request = read_request(&mut reader, method::SESSIONS_ATTACH);
        assert_eq!(request["params"]["session_id"], "demo");
        assert!(request["params"]["since"].is_null());
        let cursor = Cursor { session_id: "demo".into(), seq: "3".into() };
        reply(
            &mut peer,
            &request,
            Attached::Snapshot {
                subscription_id: "demo-subscription".into(),
                snapshot: SnapshotLease { token: "lease".into(), at: cursor.clone(), expires_at: "9999999999999".into() },
            },
        );
        // Both arrive while the immutable snapshot is still loading.
        for notification in [demo::append(3, "duplicate"), demo::append(4, " live 🦀")] {
            writeln!(peer, "{}", json!({"jsonrpc":"2.0","method":method::SESSION_EVENT,"params":notification})).unwrap();
        }
        let request = read_request(&mut reader, method::SESSIONS_SNAPSHOT);
        assert_eq!(request["params"]["token"], "lease");
        reply(
            &mut peer,
            &request,
            SnapshotPage {
                at: cursor.clone(),
                records: vec![Record::Session { session: source.session.clone().unwrap() }],
                next_page: Some("first-fragment".into()),
                fragment: None,
                expires_at: None,
            },
        );
        let record = serde_json::to_string(&Record::Item { item: source.item("agent").unwrap().clone() }).unwrap();
        let cut = record.char_indices().nth(record.chars().count() / 2).unwrap().0;
        for (page, offset, fragment, last) in [("first-fragment", 0, &record[..cut], false), ("last-fragment", cut, &record[cut..], true)] {
            let request = read_request(&mut reader, method::SESSIONS_SNAPSHOT);
            assert_eq!(request["params"]["page"], page);
            reply(
                &mut peer,
                &request,
                SnapshotPage {
                    at: cursor.clone(),
                    records: vec![],
                    next_page: (!last).then(|| "last-fragment".into()),
                    fragment: Some(RecordFragment { record: "agent-record".into(), offset: offset.to_string(), json: fragment.into(), last }),
                    expires_at: None,
                },
            );
        }
    });
    let client = client_over(socket);
    let mut view = silli_client::Conversation::attach(&client, "demo").unwrap();
    assert_eq!(view.seq, 3);
    for expected in [false, true] {
        let (_, params) = client.notifications.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(view.accept(serde_json::from_value(params).unwrap()).unwrap().is_some(), expected);
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
    let _ = app.update(Message::Lane("other".into(), current + 7, silli_client::lane::Update::Snapshot(Box::new(stale))));
    let _ = app.update(Message::Lane("other".into(), current + 7, silli_client::lane::Update::Gone("old lane".into())));
    assert!(app.views["other"].conversation.items().is_empty());
    assert_eq!(app.views["other"].status, silli_client::lane::Status::Live);
}

// --- the person's requests ----------------------------------------------------------------------

/// Each action is one public SFP request of the exact shape the daemon accepts (its request
/// structs refuse unknown fields): send, stop, answer, a mode, an option, unqueue.
#[test]
fn actions_send_the_protocol_requests() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    // an empty composer sends nothing
    let task = app.update(in_session(&app, session::Event::Send));
    settle_task(&mut app, task);
    type_text(&mut app, "hello agent\nsecond line");
    let task = app.update(in_session(&app, session::Event::Send));
    assert!(demo_view(&app).draft.text().trim().is_empty(), "the composer clears on send");
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    let params = &request["params"];
    assert_eq!(params["session_id"], "demo");
    assert_eq!(params["text"], "hello agent\nsecond line");
    assert_eq!(params["attachments"], json!([]));
    assert!(params["submission_id"].as_str().unwrap().starts_with("gui-prompt-"));
    serde_json::from_value::<Submit>(params.clone()).unwrap();
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).outbox.pending().is_empty(), "accepted");

    let task = app.update(in_session(&app, session::Event::Stop));
    let request = daemon.expect(method::SESSIONS_CANCEL);
    assert_eq!(request["params"], json!({"session_id":"demo", "turn_id":"demo-turn"}));
    daemon.reply(&request, json!({"requested": true}));
    settle_task(&mut app, task);

    let task = app.update(in_session(&app, session::Event::Answer { question_id: "q1".into(), option_id: "allow".into() }));
    let again = app.update(in_session(&app, session::Event::Answer { question_id: "q1".into(), option_id: "allow".into() }));
    let request = daemon.expect(method::QUESTIONS_ANSWER);
    assert_eq!(request["params"]["answer"], json!({"type":"permission", "option_id":"allow"}));
    assert!(request["params"]["answer_id"].as_str().unwrap().starts_with("gui-answer-"));
    serde_json::from_value::<AnswerQuestion>(request["params"].clone()).unwrap();
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    settle_task(&mut app, again);

    let pick = |value: &str, boolean| crate::Pick { value: value.into(), title: value.into(), boolean };
    let task = app.update(in_session(&app, session::Event::SetMode(pick("plan", None))));
    let request = daemon.expect(method::SESSIONS_CONTROL);
    assert_eq!(request["params"]["control"], json!({"type":"set_mode", "mode_id":"plan"}), "a second press on an answer in flight sent nothing");
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    let task = app.update(in_session(&app, session::Event::SetOption("web".into(), pick("true", Some(true)))));
    let request = daemon.expect(method::SESSIONS_CONTROL);
    assert_eq!(request["params"]["control"], json!({"type":"set_config", "option_id":"web", "value":{"type":"bool", "value":true}}));
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);

    let task = app.update(in_session(&app, session::Event::Unqueue("s1".into())));
    let request = daemon.expect(method::SUBMISSIONS_CANCEL);
    assert_eq!(request["params"], json!({"session_id":"demo", "submission_id":"s1"}));
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).failed.is_none());
}

/// Send now asks the daemon to send a queued prompt next (it stops the turn itself), only of a
/// daemon that answered `silli.send_now`; an older one is told apart without a request.
#[test]
fn send_now_needs_the_capability() {
    let (older, _quiet) = daemon();
    let mut app = live_app(vec![older]);
    let task = app.update(in_session(&app, session::Event::SendNow("s1".into())));
    settle_task(&mut app, task);
    let failed = demo_view(&app).failed.clone().expect("refused here");
    assert!(failed.contains("silli.send_now"), "{failed}");

    let (client, mut daemon) = daemon();
    let initializing = std::thread::spawn(move || {
        client.initialize().unwrap();
        client
    });
    let request = daemon.expect(silli_proto::method::INITIALIZE);
    assert_eq!(request["params"]["capabilities"]["silli.send_now"], 1, "offered");
    let answer = silli_proto::initialize::Initialized {
        protocol: silli_proto::initialize::PROTOCOL_ID.into(),
        daemon: silli_proto::initialize::Peer { name: "silli".into(), version: "test".into() },
        capabilities: [("silli.send_now".to_string(), 1)].into(),
        limits: Default::default(),
    };
    daemon.reply(&request, serde_json::to_value(answer).unwrap());
    let client = initializing.join().unwrap();
    let mut app = live_app(vec![client]);
    let task = app.update(in_session(&app, session::Event::SendNow("s1".into())));
    let request = daemon.expect(method::SUBMISSIONS_SEND_NOW);
    assert_eq!(request["params"], json!({"session_id":"demo", "submission_id":"s1"}));
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
    let task = app.update(in_session(&app, session::Event::Stop));
    let first = daemon.expect(method::SUBMISSIONS_CANCEL);
    assert_eq!(first["params"]["submission_id"], "waiting");
    daemon.reply(&first, json!({}));
    let second = daemon.expect(method::SESSIONS_CANCEL);
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
    let task = app.update(in_session(&app, session::Event::Send));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    daemon.refuse(&request, "invalid_state", "the session has ended");
    settle_task(&mut app, task);
    let view = demo_view(&app);
    assert_eq!(view.draft.text().trim_end(), "keep me");
    assert!(view.failed.as_ref().unwrap().contains("the session has ended"));
    assert!(view.outbox.pending().is_empty());

    // typed again before the refusal came back: the new text stays, the old prompt waits
    let task = app.update(in_session(&app, session::Event::Send));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    assert_eq!(request["params"]["text"], "keep me");
    let id = request["params"]["submission_id"].as_str().unwrap().to_string();
    type_text(&mut app, "newer");
    daemon.refuse(&request, "invalid_state", "the session has ended");
    settle_task(&mut app, task);
    let view = demo_view(&app);
    assert_eq!(view.draft.text().trim_end(), "newer");
    assert!(matches!(view.outbox.get(&id).unwrap().state, State::Failed(_)));
    let _ = app.update(in_session(&app, session::Event::EditSend(id)));
    assert_eq!(demo_view(&app).draft.text().trim_end(), "newer\n\nkeep me");

    // the connection goes with the request in hand: uncertain, not resent
    let _ = app.update(in_session(&app, session::Event::Prompt(iced::widget::text_editor::Action::SelectAll)));
    let _ = app.update(in_session(&app, session::Event::Prompt(iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Delete))));
    type_text(&mut app, "did it arrive?");
    let task = app.update(in_session(&app, session::Event::Send));
    // Read on the existing connection before closing it. Dropping it before Send raced
    // the client's EOF detection and sometimes never reached the expected next socket.
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    drop(daemon);
    settle_task(&mut app, task);
    let id = request["params"]["submission_id"].as_str().unwrap().to_string();
    assert!(matches!(demo_view(&app).outbox.get(&id).unwrap().state, State::Uncertain(_)));
    assert!(demo_view(&app).draft.text().trim().is_empty());
    // checking asks for it by identity; never received, it comes back to the composer
    let task = app.update(in_session(&app, session::Event::CheckSend(id.clone())));
    let request = checker.expect(method::SUBMISSIONS_GET);
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
    assert!(ui.find("for the demo agent only").is_err(), "not drawn in the other tab");
    drop(ui);
    let _ = app.update(in_session(&app, session::Event::Send));
    assert!(app.views["other"].outbox.pending().is_empty(), "nothing to send there");
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(demo_view(&app).draft.text().trim_end(), "for the demo agent only");
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
    assert!(ui.find("Waiting for your answer: Edit conversation.rs").is_err());
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
    let _ =
        app.update(Message::Files(Action::Open { path: silli_proto::sessions::HostPath::from_bytes("/home/demo/project/README.md".as_bytes()), repo: None }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("other-title").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::Choose(id)] if id == "other"), "{messages:?}");
}

// --- rendering and input ------------------------------------------------------------------------

#[test]
fn bundled_faces_resolve_at_normal_and_bold_weights() {
    use iced::advanced::graphics::text::{cosmic_text::fontdb, font_system};
    crate::fonts::install();
    let mut system = font_system().write().unwrap();
    for family in [crate::fonts::SANS_FAMILY, crate::fonts::MONO_FAMILY] {
        for weight in [fontdb::Weight::NORMAL, fontdb::Weight::BOLD] {
            let query = fontdb::Query { families: &[fontdb::Family::Name(family)], weight, ..fontdb::Query::default() };
            let id = system.raw().db().query(&query).expect("a face");
            let face = system.raw().db().face(id).unwrap();
            assert!(face.families.iter().any(|(name, _)| name == family));
            assert_eq!(face.weight, weight, "{family} at {weight:?}");
        }
    }
}

/// Enter sends from the focused composer; Shift+Enter is a new line, which continues a list.
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
    assert!(matches!(crate::ui::enter_sends("demo", press(Modifiers::empty())), Some(Binding::Custom(Message::Session(_, session::Event::Send)))));
    assert!(matches!(crate::ui::enter_sends("demo", press(Modifiers::SHIFT)), Some(Binding::Custom(Message::Session(_, session::Event::PromptNewline)))));
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
            Message::Session(_, session::Event::Answer { question_id, option_id }) if question_id == "demo-question" && option_id == "allow")),
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
    view.accept(event(&view, Event::TurnUpdated { turn: turn.clone() })).unwrap();
    assert_eq!(view.turn_end("demo-turn").as_deref(), Some("stopped"));
    let mut done = turn;
    done.outcome = "completed".into();
    view.accept(event(&view, Event::TurnUpdated { turn: done })).unwrap();
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
    assert!(matches!(messages.first(), Some(Message::DragWindow)), "{messages:?}");
    assert!(matches!(messages.get(1), Some(Message::Choose(id)) if id == "demo"), "{messages:?}");
    assert_eq!(messages.len(), 2, "a tab click must not also move the window: {messages:?}");
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
        assert!(matches!(resized(&press(&app, 718.0, y)), Some(Direction::East)), "the right edge, its corners included, changes the width only ({y})");
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
    assert!(matches!(crate::keys::shortcut(press(Modifiers::COMMAND)), Some(Message::Quit)));
    assert!(crate::keys::shortcut(press(Modifiers::empty())).is_none());
}

// --- the terminal -------------------------------------------------------------------------------

/// Wait until the pane's screen shows `text` (its reader thread fills it from the stream).
pub fn settle(pane: &crate::terminal::Pane, text: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !pane.screen().contains(text) {
        assert!(std::time::Instant::now() < deadline, "{text:?} never appeared:\n{}", pane.screen());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A connection over `socket`, its controls recorded in `seen`.
fn connection(socket: Socket, seen: Arc<Mutex<Vec<String>>>) -> silli_client::terminal::Connection {
    use silli_client::terminal::{Connection, Control};
    let close = socket.try_clone().unwrap();
    Connection {
        discarded: true,
        size: (40, 10),
        stream: silli_proto::transport::RawStream {
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
    let (ours, mut remote) = socket_pair();
    remote.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
    assert!(screen.starts_with("[earlier output was not kept]\nhi red\n$"), "{screen}");
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
    let (ours, mut remote) = socket_pair();
    remote.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
    assert_eq!(remote.read(&mut more).unwrap(), 0, "the stream must close when its pane goes");
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
    assert_eq!(key(ch("ä"), none, Some("ä"), plain).unwrap(), "ä".as_bytes());
    assert_eq!(key(ch("c"), Modifiers::CTRL, None, plain).unwrap(), [3]);
    assert_eq!(key(ch("x"), Modifiers::ALT, Some("x"), plain).unwrap(), b"\x1bx");
    assert_eq!(key(named(Named::Enter), none, Some("\r"), plain).unwrap(), b"\r");
    assert_eq!(key(named(Named::Backspace), none, None, plain).unwrap(), [0x7f]);
    assert_eq!(key(named(Named::Tab), Modifiers::SHIFT, None, plain).unwrap(), b"\x1b[Z");
    assert_eq!(key(named(Named::ArrowUp), none, None, plain).unwrap(), b"\x1b[A");
    assert_eq!(key(named(Named::ArrowUp), none, None, TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
    assert_eq!(key(named(Named::ArrowLeft), Modifiers::CTRL, None, plain).unwrap(), b"\x1b[1;5D");
    assert_eq!(key(named(Named::Delete), none, None, plain).unwrap(), b"\x1b[3~");
    assert_eq!(key(named(Named::F5), none, None, plain).unwrap(), b"\x1b[15~");
    assert!(key(named(Named::Shift), Modifiers::SHIFT, None, plain).is_none());
}

/// The terminal button opens the shell in the prompt's place; typed keys go to it, not to the prompt.
#[test]
fn the_terminal_button_opens_the_shell_and_takes_the_keys() {
    let (mut app, _) = App::new(true, None);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("terminal-button")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(messages.as_slice(), [Message::Session(id, session::Event::ToggleTerminal)] if id == "demo"), "{messages:?}");
    // the task carries the pane's events; dropping it would hang the shell up
    let _events = app.update(Message::ToggleTerminal);
    let pane = app.views.get_mut("demo").unwrap().terminal.as_mut().unwrap();
    pane.accept(crate::terminal::Event::Opened);
    settle(pane, "project$");
    let mut ui = simulator(&app, (720.0, 860.0));
    let _ = ui.typewrite("ls");
    let typed: Vec<u8> = ui
        .into_messages()
        .filter_map(|m| match m {
            Message::Session(_, session::Event::TermAction(crate::terminal::Action::Input(bytes))) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(typed, b"ls");
    let _ = app.update(Message::ToggleTerminal);
    assert!(!demo_view(&app).terminal_shown && demo_view(&app).terminal.is_some(), "hiding only detaches the view");
}

// --- files --------------------------------------------------------------------------------------

#[test]
fn file_drawer_buttons_open_the_viewer_and_return_to_conversation() {
    use crate::files::Action;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(crate::layout::Mode::Drawer)));
    let mut ui = simulator(&app, (1000.0, 860.0));
    ui.click("README.md").unwrap();
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::Files(Action::Open { .. }))));
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
        vec![(None, None, "@@ -3,2 +5,3 @@"), (Some(3), Some(5), " keep"), (Some(4), None, "-old"), (None, Some(6), "+new"), (None, Some(7), "+extra")]
    );
    assert!(numbered_diff("(untracked)\n- list item\n+ another item").iter().all(|(old, new, _)| old.is_none() && new.is_none()));
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

/// The agent's controls take all the room the composer bar's buttons leave: whole when they fit,
/// and scrolling only once the bar is full up to the buttons at its left (a fixed allowance for
/// the buttons cut them short with room to spare).
#[test]
fn the_agents_controls_use_the_whole_composer_bar() {
    use iced_test::selector::Candidate;
    let (app, _) = App::new(true, None);
    let measure = |width: f32| {
        let mut ui = simulator(&app, (width, 700.0));
        let controls = ui
            .select(|candidate| match candidate {
                Candidate::Scrollable { id: Some(id), bounds, content_bounds, .. } if id == &iced::widget::Id::new("agent-controls") => {
                    Some((bounds, content_bounds))
                }
                _ => None,
            })
            .expect("the agent's controls");
        let shell = ui
            .select(|candidate| match candidate {
                Candidate::Container { id: Some(id), bounds, .. } if id == &iced::widget::Id::new("terminal-button") => Some(bounds),
                _ => None,
            })
            .expect("the shell's button");
        (controls.0, controls.1, shell)
    };
    let (shown, content, _) = measure(720.0);
    assert!(shown.width >= content.width - 0.5, "all in view with room: {shown:?} of {content:?}");
    // narrower and narrower: whenever they are cut, no room is left between them and the buttons
    let mut cut = false;
    for width in (200..=720).rev().step_by(20) {
        let (shown, content, shell) = measure(width as f32);
        if shown.width < content.width - 0.5 {
            cut = true;
            let gap = shown.x - (shell.x + shell.width);
            assert!(gap < 40.0, "at {width} px the controls are cut with {gap} px unused");
        }
    }
    assert!(cut, "no width cut the controls: the test measures nothing");
}

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
    live.apply(&mut app, Message::Files(crate::files::Action::ToggleDrawer));
    assert!(app.narrow(), "520 px does not fit the drawer beside");
    let mut after = None;
    live.frame(&app, |ui| after = scrolled(ui, "conversation"));
    let (before, after) = (before.unwrap(), after.unwrap());
    assert!(before > 100.0, "the conversation is longer than the window");
    // Opening the drawer keeps the conversation at its tail.
    assert!(after >= before - 1.0, "back towards the top: {before} → {after}");
    assert!(app.current().unwrap().follow_tail);
}

/// The keys go between the modes: Ctrl+Shift+D from Shelf to Drawer and back, Ctrl+Shift+F into
/// Full and back to where it came from; putting Drawer mode's drawer away is Full; in
/// Shelf mode a chip or the drawer's toggle is a quick look over the conversation.
#[test]
fn the_keys_go_between_shelf_drawer_and_full() {
    use crate::files::Action as Files;
    use crate::layout::Mode;
    let (mut app, _) = App::new(true, None);
    assert_eq!(app.layout.mode, Mode::Shelf);
    let press = |c: &str| {
        crate::keys::shortcut(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Character(c.into()),
            modified_key: iced::keyboard::Key::Character(c.into()),
            physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT,
            text: None,
            repeat: false,
        })
    };
    assert!(press("E").is_none(), "Ctrl+Shift+E is free again");
    let _ = app.update(Message::Files(Files::ToggleDrawer));
    assert!(app.drawer.shown, "a quick look in Shelf mode");
    assert!(matches!(app.layout.drawer(true, 1200.0), crate::layout::Drawer::Over(_)));
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
            Candidate::Custom { state, .. } => state.downcast_ref::<crate::selection::Selectable>().filter(|t| t.text().contains(needle)).map(|_| ()),
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
            Candidate::Custom { state, .. } => state.downcast_ref::<crate::selection::Selectable>().filter(|t| t.text().contains(needle)).map(|_| ()),
            _ => None,
        })
        .is_some()
    };
    let height = |ui: &mut Frame<'_, '_>| -> Option<f32> {
        ui.select(|candidate| match candidate {
            Candidate::Scrollable { id: Some(found), content_bounds, .. } if *found == iced::widget::Id::new("conversation") => Some(content_bounds.height),
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
    assert!((full - tail).abs() < 1.0, "as tall with gaps: {full} vs {tail}");
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
    assert!(matches!(page.snapshot, Some(Ok(_))), "read at once in the demo");
    for (category, labels) in
        [("Interface", vec!["Keyboard shortcuts"]), ("Sessions", vec!["Timer autoshow", "Remember pins"]), ("Memory & history", vec!["Compact after"])]
    {
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
    assert!(app.settings_page.is_none(), "Escape: back to the conversation");
    assert_ne!(app.herring_flick, flicked, "and again on the way back");
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    assert!(app.settings_page.is_some(), "the herring opens the page last shown");
    // the theme shows before the daemon answers
    let before = crate::style::theme_id();
    let _ = app.update(Message::Settings(crate::settings_ui::Event::Change(silli_proto::admin::SettingChange::Theme { value: "dracula".into() })));
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
        let renderer =
            block_on(<iced::Renderer as Headless>::new(settings.default_font, settings.default_text_size, TEST_BACKEND)).expect("a headless renderer");
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
        let mut ui = iced_runtime::UserInterface::build(app.view(), self.size, self.cache.take().unwrap_or_default(), &mut self.renderer);
        // as the runtime does: an operation may chain the next (focus traversal counts first)
        for operation in self.operations.drain(..) {
            let mut current = Some(operation);
            while let Some(mut operation) = current.take() {
                ui.operate(&self.renderer, operation.as_mut());
                if let iced::advanced::widget::operation::Outcome::Chain(next) = operation.finish() {
                    current = Some(next);
                }
            }
        }
        let mut frame = Frame { ui: &mut ui, renderer: &mut self.renderer, cursor: &mut self.cursor, messages: Vec::new() };
        drive(&mut frame);
        let messages = frame.messages;
        ui.draw(&mut self.renderer, &crate::style::theme(), &iced::advanced::renderer::Style { text_color: crate::style::P.fg }, self.cursor);
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
        let _ = self.ui.update(&events, *self.cursor, self.renderer, &mut iced::advanced::clipboard::Null, &mut self.messages);
    }

    fn select<T, F>(&mut self, selector: F) -> Option<T>
    where
        T: Clone + Send + 'static,
        F: FnMut(iced_test::selector::Candidate<'_>) -> Option<T> + Send,
    {
        use iced::advanced::widget::Operation;
        use iced_test::selector::Selector;
        let mut operation = selector.find();
        self.ui.operate(self.renderer, &mut iced::advanced::widget::operation::black_box(&mut operation));
        match operation.finish() {
            iced::advanced::widget::operation::Outcome::Some(found) => found,
            _ => None,
        }
    }
}

/// How far the scrollable `id` is scrolled down.
fn scrolled(ui: &mut impl Drive, id: &'static str) -> Option<f32> {
    ui.select(move |candidate| match candidate {
        iced_test::selector::Candidate::Scrollable { id: Some(found), translation, .. } if *found == iced::widget::Id::new(id) => Some(translation.y),
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
        let Candidate::Custom { bounds, visible_bounds: Some(visible), state, .. } = candidate else {
            return None;
        };
        let text = state.downcast_ref::<crate::selection::Selectable>()?;
        if !text.text().starts_with(leaf.as_str()) {
            return None;
        }
        assert!((visible.width - bounds.width).abs() < 0.5 && (visible.height - bounds.height).abs() < 0.5, "{leaf:?} is not all on screen");
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
    ui.send(vec![iced::Event::Mouse(Event::CursorMoved { position: at }), iced::Event::Mouse(Event::ButtonPressed(Button::Left))]);
}

fn move_to(ui: &mut impl Drive, at: iced::Point) {
    ui.aim(at);
    ui.send(vec![iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: at })]);
}

fn release_at(ui: &mut impl Drive, at: iced::Point) {
    use iced::mouse::{Button, Event};
    ui.aim(at);
    ui.send(vec![iced::Event::Mouse(Event::CursorMoved { position: at }), iced::Event::Mouse(Event::ButtonReleased(Button::Left))]);
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
    iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: lines } })
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
    iced_runtime::task::into_stream(task)
        .is_some_and(|stream| block_on(stream.collect::<Vec<_>>()).iter().any(|action| matches!(action, iced_runtime::Action::Widget(_))))
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

/// The gate (docs/silli-gui.md, direction 5): a drag from the person's prompt into the agent's
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
    assert_eq!(lit.first().map(String::as_str), Some("smaller and show me the change."));
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
          fn main() {\n    println!(\"Silli GUI\");\n}\n\
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
    assert!(pressed.iter().any(|m| matches!(m, Message::Selecting(true))), "{pressed:?}");
    for message in pressed {
        live.apply(&mut app, message);
    }
    assert!(app.selecting);
    assert!(!snaps(app.update(Message::DemoTick)), "a streamed chunk");
    let seq = demo_view(&app).conversation.seq + 1;
    let event = silli_client::lane::Update::Event(Box::new(demo::append(seq, " More.")));
    assert!(!snaps(app.update(Message::Lane("demo".into(), 0, event))), "an event from the lane");
    assert!(demo_view(&app).follow_tail);
    let released = live.frame(&app, |ui| {
        let to = aim_at(ui, "A streamed paragraph", "streamed");
        move_to(ui, to);
        release_at(ui, to);
        ui.send(ctrl_c());
    });
    assert!(released.iter().any(|m| matches!(m, Message::Selecting(false))), "{released:?}");
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
    assert_eq!(copied, ["send arrow a little smaller and show me the change.\nThe arrow's size is one "]);
    for message in made {
        live.apply(&mut app, message);
    }
    for _ in 1..=demo::CHUNKS.len() {
        live.apply(&mut app, Message::DemoTick);
        let again = live.frame(&app, |ui| ui.send(ctrl_c()));
        assert_eq!(copies(again), copied);
    }
}

/// Letting go of the tail is a matter of pixels, not of a share of the conversation: any move up
/// from the end stops following, however long the conversation is; new text below and a move
/// that is not towards the end change nothing; scrolling down to near the end takes it up again.
#[test]
fn a_small_scroll_up_lets_go_of_the_tail() {
    use crate::session::follows;
    // 12 px up from the end of a conversation of any length
    assert!(!follows(true, 50_000.0, 49_988.0, 12.0));
    // new text below: the offset stands, the end moved away
    assert!(follows(true, 500.0, 500.0, 300.0));
    assert!(!follows(false, 500.0, 500.0, 300.0));
    // shorter content keeps the view at its end
    assert!(follows(true, 500.0, 420.0, 0.0));
    // scrolling in the middle
    assert!(!follows(false, 500.0, 560.0, 200.0));
    // back down to near the end, and to the end itself
    assert!(follows(false, 500.0, 560.0, 20.0));
    assert!(follows(false, 500.0, 580.0, 0.0));
}

/// Text that arrives just as the person scrolls up does not pull the conversation back: the
/// scroll to the end it asked for finds the conversation moved and leaves it.
#[test]
fn new_text_does_not_undo_a_scroll_away_from_the_tail() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((720.0, 400.0));
    live.apply(&mut app, Message::Latest);
    let redraw = || iced::Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now()));
    // the conversation reports where its end is
    for message in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, message);
    }
    assert!(demo_view(&app).follow_tail);
    let mut at_end = None;
    let scrolled_up = live.frame(&app, |ui| {
        at_end = scrolled(ui, "conversation");
        ui.aim(iced::Point::new(360.0, 200.0));
        ui.send(vec![iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta: iced::mouse::ScrollDelta::Pixels { x: 0.0, y: 12.0 } })]);
    });
    // new text is handled before the scroll's own message
    let seq = demo_view(&app).conversation.seq + 1;
    let event = silli_client::lane::Update::Event(Box::new(demo::append(seq, " More.")));
    live.apply(&mut app, Message::Lane("demo".into(), 0, event));
    for message in scrolled_up {
        live.apply(&mut app, message);
    }
    assert!(!demo_view(&app).follow_tail, "12 px up lets go of the tail");
    let mut now = None;
    live.frame(&app, |ui| now = scrolled(ui, "conversation"));
    let (at_end, now) = (at_end.unwrap(), now.unwrap());
    assert!(at_end > 100.0, "the conversation is longer than the window");
    assert!((at_end - 12.0 - now).abs() < 1.0, "left where it was scrolled to: {now} of {at_end}");
    // and following still works: back at the end, new text scrolls
    live.apply(&mut app, Message::Latest);
    for message in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, message);
    }
    let seq = demo_view(&app).conversation.seq + 1;
    let more = "\n\nAnother paragraph.\n\nAnd one more.\n\nAnd a last one.";
    let event = silli_client::lane::Update::Event(Box::new(demo::append(seq, more)));
    live.apply(&mut app, Message::Lane("demo".into(), 0, event));
    let mut later = None;
    for message in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, message);
    }
    live.frame(&app, |ui| later = scrolled(ui, "conversation"));
    assert!(demo_view(&app).follow_tail);
    assert!(later.unwrap() > at_end + 20.0, "the tail was followed: {later:?} after {at_end}");
}

/// Scrolling up from the tail turns following off and puts "↓ Latest" over the conversation;
/// the conversation stays where it was scrolled to (it used to start again at its top).
#[test]
fn scrolling_up_from_the_tail_keeps_the_place() {
    let mut app = streamed();
    let _ = app.update(Message::DemoTick);
    let mut live = Live::new((720.0, 400.0));
    live.apply(&mut app, Message::Latest);
    // the conversation reports where its end is, as it does when the window draws it
    let redraw = iced::Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now()));
    for message in live.frame(&app, |ui| ui.send(vec![redraw])) {
        live.apply(&mut app, message);
    }
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
    assert!((at_end - 60.0 - now).abs() < 1.0, "one notch above the end: {now}");
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
    live.apply(&mut app, Message::Layout(crate::layout::Action::Mode(crate::layout::Mode::Drawer)));
    assert!(app.drawer.shown);
    let mut after = None;
    live.frame(&app, |ui| after = scrolled(ui, "conversation"));
    let (before, after) = (before.unwrap(), after.unwrap());
    assert!(before > 100.0, "the conversation is longer than the window");
    assert!((before - after).abs() < 1.0, "the place moved when the drawer opened: {before} → {after}");
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
    ui.click("rg -n 'SEND_SIZE' crates/silli-gui/src").unwrap();
    assert_eq!(highlighted(&mut ui).len(), 2);
    let elsewhere = aim_at(&mut ui, "I'll make the arrow", "arrow");
    press_at(&mut ui, elsewhere);
    release_at(&mut ui, elsewhere);
    assert!(highlighted(&mut ui).is_empty());
    ui.send(ctrl_c());
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::Session(_, session::Event::ToggleTool(id)) if id == "tool")), "{messages:?}");
    assert!(copies(messages).is_empty());
}

/// A file in the reader selects the same way: across a Markdown file's blocks, and across a
/// diff's lines without their line numbers.
#[test]
fn the_reader_selects_across_markdown_blocks_and_diff_lines() {
    use crate::files::Action;
    use silli_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_bytes(s.as_bytes());
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::Open { path: path("/home/demo/project/README.md"), repo: None }));
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
    let _ = app.update(Message::Files(Action::Open { path: path("/home/demo/project/README.md"), repo: Some(path("/home/demo/project")) }));
    let mut ui = simulator(&app, (1000.0, 860.0));
    let from = aim_at(&mut ui, "-A conversation preview.", "-A");
    let to = aim_at(&mut ui, "+Files and diffs", "Files");
    drag(&mut ui, from, to);
    ui.send(ctrl_c());
    assert_eq!(copies(ui.into_messages()), ["-A conversation preview.\n+A place to work with your agents.\n+"]);
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
fn at_each_stop(app: &mut App, size: (f32, f32), steps: usize, press: iced::keyboard::key::Named) -> Vec<Option<Message>> {
    let mut live = Live::new(size);
    live.frame(app, |_| {});
    (0..steps)
        .map(|_| {
            live.apply(app, Message::Focus(true));
            live.frame(app, |ui| ui.send(vec![key(press)])).into_iter().next()
        })
        .collect()
}

/// The keyboard gate (docs/silli-gui.md, direction 6): Tab reaches the controls in reading order —
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
            Some(Message::Files(action)) => format!("files {action:?}").chars().take(24).collect(),
            Some(Message::Session(_, session::Event::Answer { option_id, .. })) => format!("answer {option_id}"),
            Some(Message::Session(_, event)) => format!("{event:?}").chars().take(24).collect(),
            Some(other) => format!("{other:?}").chars().take(24).collect(),
            None => "-".into(),
        })
        .collect();
    let at = |what: &str| names.iter().position(|n| n.starts_with(what)).unwrap_or_else(|| panic!("{what} not reached: {names:?}"));
    assert_eq!(names[0], "ToggleChanges", "{names:?}");
    assert_eq!(names[1], "Details(Open(\"demo\"))", "{names:?}");
    assert_eq!(names[2], "choose demo", "{names:?}");
    let order = [at("files Toggle("), at("ToggleTool(\"tool\")"), at("answer allow"), at("answer reject"), at("Send"), at("ToggleTerminal"), at("Stop")];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?} in {names:?}");
}

/// Space presses too, and a dropdown steps through its choices with Up and Down.
#[test]
fn space_presses_and_arrows_step_a_dropdown() {
    use iced::keyboard::key::Named;
    let mut app = streamed();
    let spaced = at_each_stop(&mut app, (720.0, 860.0), 3, Named::Space);
    assert!(matches!(&spaced[2], Some(Message::Choose(id)) if id == "demo"), "{spaced:?}");
    let stepped = at_each_stop(&mut app, (720.0, 860.0), 40, Named::ArrowDown);
    assert!(
        stepped.iter().any(|m| matches!(m, Some(Message::Session(_, session::Event::SetMode(pick))) if pick.value == "plan")),
        "the mode dropdown takes Down: {stepped:?}"
    );
    assert!(
        stepped.iter().any(|m| matches!(m, Some(Message::Session(_, session::Event::SetOption(option, pick))) if option == "model" && pick.value == "brisk")),
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
    assert!(offsets.iter().any(|y| *y > 50.0), "tabbing into the conversation scrolled it: {offsets:?}");
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
    assert!(matches!(shortcut(press(named(Named::Tab), Modifiers::empty())), Some(Message::Focus(true))));
    assert!(matches!(shortcut(press(named(Named::Tab), Modifiers::SHIFT)), Some(Message::Focus(false))));
    assert!(matches!(shortcut(press(ch("."), Modifiers::COMMAND)), Some(Message::StepTab(1))));
    assert!(matches!(shortcut(press(ch(","), Modifiers::COMMAND)), Some(Message::StepTab(-1))));
    assert!(matches!(shortcut(press(ch("F"), both)), Some(Message::Layout(crate::layout::Action::ToggleFull))));
    assert!(matches!(shortcut(press(ch("t"), Modifiers::COMMAND)), Some(Message::NewSession)));
    assert!(matches!(shortcut(press(named(Named::Escape), Modifiers::empty())), Some(Message::Escape)));
    assert!(matches!(shortcut(press(named(Named::PageDown), Modifiers::empty())), Some(Message::Page(true))));
    assert!(matches!(shortcut(press(named(Named::End), Modifiers::COMMAND)), Some(Message::Latest)));
    assert!(matches!(shortcut(press(ch("q"), Modifiers::COMMAND)), Some(Message::Quit)));
    assert!(shortcut(press(ch("a"), Modifiers::empty())).is_none());

    assert!(matches!(shortcut(press(ch("w"), Modifiers::COMMAND)), Some(Message::CloseCurrent)));
    for digit in 1..=9 {
        assert!(matches!(shortcut(press(ch(&digit.to_string()), Modifiers::COMMAND)),
            Some(Message::SelectTab(at)) if at == digit - 1));
    }
    // Ctrl+0 is no tab: it puts Interface scale back to 100 %, as + and - step it
    for (key, step) in [("0", 0), ("+", 1), ("=", 1), ("-", -1)] {
        assert!(matches!(shortcut(press(ch(key), Modifiers::COMMAND)), Some(Message::ScaleStep(s)) if s == step), "Ctrl+{key}");
    }
    assert!(matches!(shortcut(press(ch("t"), both)), Some(Message::ToggleTerminal)));
    assert!(matches!(shortcut(press(ch("T"), both)), Some(Message::ToggleTerminal)));
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
        crate::ui::enter_sends("demo", typed(named(Named::ArrowLeft), JUMP | Modifiers::SHIFT)),
        Some(Binding::Select(iced::widget::text_editor::Motion::WordLeft))
    ));
    assert!(matches!(crate::ui::enter_sends("demo", typed(named(Named::PageUp), Modifiers::empty())), Some(Binding::Custom(Message::Page(false)))));
    // Tab is the prompt's: it indents in a list and moves the keyboard on anywhere else
    assert!(matches!(
        crate::ui::enter_sends("demo", typed(named(Named::Tab), Modifiers::empty())),
        Some(Binding::Custom(Message::Session(_, session::Event::PromptIndent(false))))
    ));
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
    use crate::files::Action;
    use crate::layout::Mode;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(Mode::Full)));
    let _ = app.update(Message::Files(Action::ToggleDrawer));
    let _ =
        app.update(Message::Files(Action::Open { path: silli_proto::sessions::HostPath::from_bytes("/home/demo/project/README.md".as_bytes()), repo: None }));
    let _ = app.update(Message::Escape);
    assert!(demo_view(&app).files.reader.is_none(), "an open file closes first");
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

pub(crate) fn session_record(id: &str, title: &str, ended: bool, ghost: bool) -> silli_proto::sessions::Session {
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
        serde_json::from_value(
            serde_json::json!({"id": "claude", "name": "Claude Code", "command": ["claude-acp"], "home_paths": [{"path": "~/.claude", "kind": "directory"}]}),
        )
        .unwrap(),
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
    app.all_sessions = (0..7).map(|i| session_record(&format!("old{i}"), &format!("Ended session {i}"), true, false)).collect();
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

/// A box pinned in Settings has a row in the menu, which opens a window on it.
#[test]
fn the_menu_offers_pinned_boxes() {
    use crate::menu::Event;
    use silli_proto::transport::BoxTarget;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(Event::Toggle));
    let target = |host: &str| BoxTarget { host: host.into(), binary: "silli".into(), ssh_config: None };
    app.boxes.saved = Ok([("lab".to_string(), target("me@lab")), ("server".to_string(), target("me@server"))].into());
    app.settings.pinned_boxes = vec!["lab".into()];
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("me@lab").unwrap();
    assert!(ui.find("me@server").is_err(), "not pinned");
    ui.click("lab").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::Menu(Event::OpenBox(Some(name)))] if name == "lab"), "{messages:?}");
}

/// Starting from the menu creates the session with a fresh identity and opens it once the list
/// shows it; deleting asks for a second press.
#[test]
fn the_menu_starts_a_session_and_opens_it() {
    use crate::menu::Event;
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let server = std::thread::spawn(move || {
        let create = daemon.expect(method::SESSIONS_CREATE);
        let params = &create["params"];
        assert_eq!((params["agent"].as_str(), params["ghost"].as_bool()), (Some("codex"), Some(true)));
        assert_eq!(params["policy"], "default");
        assert!(params["create_id"].as_str().unwrap().starts_with("gui-create-"));
        daemon.reply(&create, json!({"session_id": "fresh"}));
        let list = daemon.expect(method::SESSIONS_LIST);
        daemon.reply(
            &list,
            json!({"sessions": [session_record("fresh", "new one", false, true),
                                session_record("demo", "witty-willow", false, false)],
                   "next": null}),
        );
        let delete = daemon.expect(method::SESSIONS_DELETE);
        assert_eq!(delete["params"], json!({"session_id": "old"}));
        daemon.reply(&delete, json!({}));
        let list = daemon.expect(method::SESSIONS_LIST);
        daemon.reply(&list, json!({"sessions": [], "next": null}));
    });
    let task = app.update(Message::Menu(Event::New { agent: "codex".into(), ghost: true, workspaces: None }));
    settle_task(&mut app, task);
    assert_eq!(app.selected.as_deref(), Some("fresh"), "opened once listed");
    app.menu = Some(crate::menu::Menu::default());
    let first = app.update(Message::Menu(Event::Delete("old".into())));
    settle_task(&mut app, first);
    assert!(app.menu.as_ref().unwrap().armed.is(&"old".to_string()));
    let second = app.update(Message::Menu(Event::Delete("old".into())));
    settle_task(&mut app, second);
    server.join().unwrap();
}

#[test]
fn always_apply_setting_only_affects_default_agent_starts() {
    use crate::menu::Event;
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let cases = [(false, None, false), (false, None, true), (true, None, false), (false, Some(false), false), (false, Some(true), false)];
    let server = std::thread::spawn(move || {
        for (ghost, workspaces, always_apply) in cases {
            let create = daemon.expect(method::SESSIONS_CREATE);
            let params = &create["params"];
            assert_eq!(params["ghost"].as_bool(), Some(ghost));
            assert_eq!(params["workspaces"].as_bool(), workspaces);
            assert_eq!(params["always_apply"].as_bool().unwrap_or(false), always_apply);
            daemon.reply(&create, json!({"session_id": "demo"}));
            let list = daemon.expect(method::SESSIONS_LIST);
            daemon.reply(&list, json!({"sessions": [session_record("demo", "witty-willow", false, false)], "next": null}));
        }
    });
    for (index, (ghost, workspaces, _)) in cases.into_iter().enumerate() {
        if index == 1 {
            app.settings_page = Some(crate::settings_ui::SettingsPage::default());
            let _ = app.update(Message::Settings(crate::settings_ui::Event::AlwaysApply(true)));
        }
        let task = app.update(Message::Menu(Event::New { agent: "codex".into(), ghost, workspaces }));
        settle_task(&mut app, task);
    }
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
    assert!(matches!(&messages[..], [Message::CloseTab(id)] if id == "demo"), "{messages:?}");
    let _ = app.update(Message::CloseTab("demo".into()));
    assert!(app.armed_close.is(&"demo".to_string()));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click(iced::widget::Id::new("close-tab")).unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::CloseTab(id)] if id == "demo"));
    let server = std::thread::spawn(move || {
        let end = daemon.expect(method::SESSIONS_END);
        assert_eq!(end["params"], json!({"session_id": "demo", "reason": "close"}));
        daemon.reply(&end, json!({"pending": false, "deleted": true}));
        let list = daemon.expect(method::SESSIONS_LIST);
        daemon.reply(&list, json!({"sessions": [], "next": null}));
    });
    let task = app.update(Message::CloseTab("demo".into()));
    settle_task(&mut app, task);
    assert!(app.tabs.is_empty() && app.selected.is_none(), "the ended session's tab went");
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
    let _ = app.update(in_session(&app, session::Event::FormEdit("form".into(), crate::forms::Edit::Text { key: "why".into(), text: "release day".into() })));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("Submit").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&messages[..], [Message::Session(_, session::Event::SubmitForm(q))] if q == "form"), "{messages:?}");
    let task = app.update(in_session(&app, session::Event::SubmitForm("form".into())));
    let answer = daemon.expect(method::QUESTIONS_ANSWER);
    assert_eq!(answer["params"]["answer"], json!({"type":"form", "values":{"env":"prod", "why":"release day"}}));
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
        matches!(&messages[..], [Message::Session(_, session::Event::ResumeHeld(a)), Message::Session(_, session::Event::DiscardHeld(b))] if a == "held1" && b == "held1"),
        "{messages:?}"
    );
    let task = app.update(in_session(&app, session::Event::ResumeHeld("held1".into())));
    let resume = daemon.expect(method::SUBMISSIONS_RESUME);
    assert_eq!(resume["params"], json!({"session_id":"demo", "submission_id":"held1"}));
    daemon.reply(&resume, json!({}));
    settle_task(&mut app, task);
}

// --- changes ----------------------------------------------------------------------------------

fn changes_key(app: &App, file: &str) -> String {
    let entries = app.changes_listing.entries.as_ref().unwrap().as_ref().unwrap();
    entries.iter().find(|e| silli_client::changes::path(e).display.ends_with(file)).map(silli_client::changes::entry_key).unwrap()
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
        ui.find(shown).unwrap_or_else(|_| panic!("{shown} is not shown"));
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
    assert!(app.changes.as_ref().unwrap().armed.is(&crate::changes_ui::Reverting::One(key.clone())));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Revert?").unwrap();
    drop(ui);
    let _ = app.update(Message::Escape);
    assert!(app.changes.is_none(), "Escape goes back to the conversation");
}

fn page_workspace(id: &str, session: &str, files: &[&str]) -> silli_proto::workspaces::Detail {
    let host = |p: &str| silli_proto::sessions::HostPath::from_bytes(p.as_bytes());
    let listed: Vec<serde_json::Value> =
        files.iter().map(|f| json!({"path": host(&format!("/home/demo/project/{f}")), "status": "M", "added": "2", "removed": "1"})).collect();
    serde_json::from_value(json!({
        "workspace": {"workspace_id": id, "name": "project", "session_id": session, "source": host("/home/demo/project"),
            "source_branch": "main", "base": "b0", "branch": format!("silli/{id}"), "path": host(&format!("/ws/{id}")),
            "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"},
        "revision": {"tip": "t1", "worktree": "w", "destination_branch": "main", "destination_tip": "b0"},
        "summary": {"commits": 1, "files": files.len(), "added": "2", "removed": "1", "uncommitted": []},
        "needs_rebase": false, "files": listed}))
    .unwrap()
}

fn page_workspaces(details: Vec<silli_proto::workspaces::Detail>) -> crate::workspaces::All {
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
    for shown in ["1 workspace to apply · 3 writes to keep or revert", "workspace", "2 files +2 −1 · 1 commit", "Apply", "Discard", "a.rs", "b.rs", "+2 −1"]
    {
        ui.find(shown).unwrap_or_else(|_| panic!("{shown} is not shown"));
    }
    assert!(ui.find("Workspaces").is_err(), "no list of workspaces apart from their sessions");
    ui.click("a.rs").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    for message in messages {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    let host = silli_proto::sessions::HostPath::from_bytes("/home/demo/project/src/a.rs".as_bytes());
    let key = workspace_key("w1", &host);
    assert!(matches!(app.changes.as_ref().unwrap().open.get(&key), Some(Some(Ok(_)))), "the file's diff is open");
    // the entries read again: a workspace's open diff is not one of theirs to close
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), vec![], 0)))));
    assert!(app.changes.as_ref().unwrap().open.contains_key(&key));
    // the workspace no longer changes that file
    let all = page_workspaces(vec![page_workspace("w1", "demo", &["src/b.rs"])]);
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(all))));
    assert!(!app.changes.as_ref().unwrap().open.contains_key(&key), "its diff went with it");
}

#[test]
fn discarded_workspaces_are_collapsed_by_default() {
    use crate::workspaces::Event as W;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let host = silli_proto::sessions::HostPath::from_bytes("/home/demo/project".as_bytes());
    let listed = serde_json::from_value(json!({
        "workspaces": [],
        "discarded": [{"discard_id": "d1", "workspace_id": "ws-old", "name": "project", "source": host, "until": "2026-11-22T10:00:00Z", "created_at": "t"}],
        "next": null,
    }))
    .unwrap();
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok((listed, vec![])))));
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("Discarded workspaces").unwrap();
    assert!(ui.find("kept until 2026-11-22").is_err(), "collapsed by default");
    ui.click("Discarded workspaces").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("kept until 2026-11-22").unwrap();
    ui.click("Discarded workspaces").unwrap();
    for message in ui.into_messages().collect::<Vec<_>>() {
        let task = app.update(message);
        settle_task(&mut app, task);
    }
    assert!(simulator(&app, (720.0, 860.0)).find("kept until 2026-11-22").is_err());
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
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes()[..3].to_vec(), vec![], 0)))));
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
    assert_eq!(app.changes.as_ref().unwrap().armed.get(), None);
    let _ = app.update(Message::Workspaces(W::PageDiscard("w1".into())));
    assert_eq!(app.changes.as_ref().unwrap().workspace_armed.get(), None);
    // the new one is read: its actions are offered
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), vec![], 0)))));
    let key = changes_key(&app, "settings.toml");
    let _ = app.update(Message::Changes(Event::Revert(key.clone())));
    assert!(app.changes.as_ref().unwrap().armed.is(&crate::changes_ui::Reverting::One(key)));
}

/// The second press confirms what the page showed at the first. A workspace that was read again
/// between the two with other work in it is asked about again, and so is one on another branch.
#[test]
fn a_workspace_that_changed_between_the_two_presses_is_asked_about_again_on_changes() {
    use crate::workspaces::Event as W;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let at = |tip: &str, branch: &str| {
        let mut detail = page_workspace("w1", "demo", &["src/a.rs"]);
        detail.revision.tip = tip.into();
        detail.revision.destination_branch = branch.into();
        page_workspaces(vec![detail])
    };
    let armed_for = |app: &App, tip: &str| {
        let page = app.changes.as_ref().unwrap();
        page.workspace_armed.get().is_some_and(|(w, seen)| w == "w1" && seen.tip == tip)
    };
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(at("t1", "main")))));
    let _ = app.update(Message::Workspaces(W::PageDiscard("w1".into())));
    assert!(armed_for(&app, "t1"));
    simulator(&app, (720.0, 860.0)).find("Discard?").unwrap();
    // read again with other work in it: the button no longer asks for the second press
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(at("t2", "main")))));
    assert!(simulator(&app, (720.0, 860.0)).find("Discard?").is_err());
    let _ = app.update(Message::Workspaces(W::PageDiscard("w1".into())));
    assert!(armed_for(&app, "t2"), "armed again, for what the page shows now");
    simulator(&app, (720.0, 860.0)).find("Discard?").unwrap();
    let _ = app.update(Message::Workspaces(W::PageDiscard("w1".into())));
    assert_eq!(app.changes.as_ref().unwrap().workspace_armed.get(), None, "the second press of the same work confirms");
    // an Apply onto another branch
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(at("t3", "feature")))));
    let _ = app.update(Message::Workspaces(W::PageApply("w1".into())));
    simulator(&app, (720.0, 860.0)).find("Apply onto feature?").unwrap();
    let _ = app.update(Message::Workspaces(W::PageLoaded(Ok(at("t4", "feature")))));
    assert!(simulator(&app, (720.0, 860.0)).find("Apply onto feature?").is_err());
    let _ = app.update(Message::Workspaces(W::PageApply("w1".into())));
    let page = app.changes.as_ref().unwrap();
    assert!(page.workspace_apply_armed.get().is_some_and(|(_, seen)| seen.tip == "t4"));
    assert_eq!(page.workspace_apply_onto, None, "not confirmed: asked again");
    let _ = app.update(Message::Workspaces(W::PageApply("w1".into())));
    let page = app.changes.as_ref().unwrap();
    assert_eq!(page.workspace_apply_armed.get(), None);
    assert_eq!(page.workspace_apply_onto, Some(("w1".to_string(), "feature".to_string())));
}

/// Revert all confirms the group the person was shown: a second press for a group that holds
/// other entries than at the first is asked about again.
#[test]
fn revert_all_of_a_group_that_changed_is_asked_about_again() {
    use crate::changes_ui::{Event, Reverting};
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("Revert all").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    let [Message::Changes(Event::RevertAll(all))] = &messages[..] else { panic!("{messages:?}") };
    assert_eq!(all.len(), 3);
    // the group as it was with one entry fewer, then as it is shown now
    let fewer = all[..2].to_vec();
    let _ = app.update(Message::Changes(Event::RevertAll(fewer.clone())));
    assert!(app.changes.as_ref().unwrap().armed.is(&Reverting::All(fewer)));
    assert!(simulator(&app, (720.0, 860.0)).find("Revert all?").is_err(), "not armed for the group shown");
    let _ = app.update(Message::Changes(Event::RevertAll(all.clone())));
    let page = app.changes.as_ref().unwrap();
    assert!(page.armed.is(&Reverting::All(all.clone())), "armed again, for the group shown now");
    assert_eq!(page.said, None, "nothing was reverted");
    simulator(&app, (720.0, 860.0)).find("Revert all?").unwrap();
    let _ = app.update(Message::Changes(Event::RevertAll(all.clone())));
    let page = app.changes.as_ref().unwrap();
    assert_eq!(page.armed.get(), None);
    assert_eq!(page.said.as_deref(), Some("The demo keeps and reverts nothing."));
}

/// The page's actions are the daemon's requests: keep with the version shown, revert all on a
/// second press, git's revert for a live file, undo for a keep; the page reads again after each.
#[test]
fn changes_actions_are_the_public_requests() {
    use crate::changes_ui::{ChangesPage, Event};
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.changes = Some(ChangesPage { fresh_entries: true, ..ChangesPage::default() });
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
                    Some(method::CHANGES_LIST) => daemon.reply(&request, json!({"entries": [], "next": null})),
                    Some(method::WORKSPACES_LIST) => daemon.reply(&request, json!({"workspaces": [], "discarded": [], "next": null})),
                    Some(method::SETTINGS_GET) => daemon.reply(&request, json!({"theme": "dracula", "auto_revert_minutes": 1440})),
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        let request = daemon.expect(method::CHANGES_KEEP);
        assert_eq!(request["params"]["session_id"], "demo");
        assert_eq!(request["params"]["items"][0]["expected_hash"], "demo-hash");
        let path = request["params"]["items"][0]["path"].clone();
        daemon.reply(&request, json!({"done": [{"path": path, "message": ""}], "failed": []}));
        reload(&mut daemon);
        let request = daemon.expect(method::CHANGES_REVERT);
        assert_eq!(request["params"]["paths"].as_array().unwrap().len(), 3);
        daemon.reply(&request, json!({"done": [], "failed": []}));
        reload(&mut daemon);
        let request = daemon.expect(method::CHANGES_GIT_REVERT);
        assert!(request["params"]["path"]["display"].as_str().unwrap().ends_with("README.md"));
        daemon.reply(&request, json!({"text": "reverted"}));
        reload(&mut daemon);
        let request = daemon.expect(method::CHANGES_UNDO);
        assert_eq!(request["params"], json!({"session_id":"demo", "keep_ids":["k1"], "force": false}));
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
    let all: Vec<String> = ["settings.toml", "plan.md", ".profile"].iter().map(|f| changes_key(&app, f)).collect();
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

/// A session's event is that session's, whichever is selected when it is handled: a Send from
/// one session's composer that arrives after the person went to another sends the first one's
/// draft and leaves the other's alone.
#[test]
fn a_send_goes_to_the_session_it_was_pressed_in() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    add_session(&mut app, "other");
    app.views.get_mut("demo").unwrap().draft = crate::editor::Editor::with_text("from demo");
    app.views.get_mut("other").unwrap().draft = crate::editor::Editor::with_text("from other");
    let pressed = in_session(&app, session::Event::Send);
    app.selected = Some("other".into());
    let task = app.update(pressed);
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    assert_eq!(request["params"]["session_id"], "demo");
    assert_eq!(request["params"]["text"], "from demo");
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(app.views["demo"].draft.text().trim().is_empty(), "the composer it was sent from clears");
    assert_eq!(app.views["other"].draft.text().trim(), "from other");
    assert!(app.views["other"].outbox.pending().is_empty());
    // a session whose view has gone: nothing happens
    let _ = app.update(Message::Session("gone".into(), session::Event::Send));
    assert_eq!(app.views["other"].draft.text().trim(), "from other");
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
    app.current_mut().unwrap().conversation.submissions.insert("queued".into(), submission.clone());
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.find("unique queued prompt").unwrap();
    ui.find("Queued 1").unwrap();
    drop(ui);
    let mut cancelled = submission;
    cancelled.outcome = "cancelled".into();
    let view = app.current_mut().unwrap();
    let update = event(&view.conversation, Event::SubmissionUpdated { submission: cancelled });
    assert!(view.accept(update));
    let mut ui = simulator(&app, (720.0, 860.0));
    assert!(ui.find("unique queued prompt").is_err());
    assert!(ui.find("Queued 1").is_err());
}

#[test]
fn workspace_shelf_preserves_prompt_focus_between_turns() {
    let (mut app, _) = App::new(true, None);
    let detail = page_workspace("w1", "demo", &["file.rs"]);
    app.current_mut().unwrap().workspaces.loaded = Some(silli_client::workspaces::Loaded {
        view: serde_json::from_value(json!({
            "workspaces_mode": true, "workspaces": [detail.workspace.clone()], "live": []
        }))
        .unwrap(),
        details: vec![detail],
    });
    type_text(&mut app, "next prompt");
    let mut live = Live::new((720.0, 860.0));
    live.operations.push(Box::new(iced::advanced::widget::operation::focusable::focus(iced::widget::Id::new("prompt"))));
    live.frame(&app, |_| {});
    for activity in ["idle", "working", "idle"] {
        app.current_mut().unwrap().conversation.session.as_mut().unwrap().activity = activity.into();
        assert_eq!(app.workspace_shelf(app.current().unwrap()).is_some(), activity == "idle");
        let messages = live.frame(&app, |ui| ui.send(vec![key(iced::keyboard::key::Named::Enter)]));
        assert!(messages.iter().any(|m| matches!(m, Message::Session(_, session::Event::Send))), "prompt lost focus with activity {activity}: {messages:?}");
    }
}

#[test]
fn sending_restores_keyboard_focus_to_the_prompt() {
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    live.frame(&app, |_| {});
    type_text(&mut app, "first prompt");
    let send = in_session(&app, session::Event::Send);
    live.apply(&mut app, send);
    type_text(&mut app, "next prompt");
    let messages = live.frame(&app, |ui| ui.send(vec![key(iced::keyboard::key::Named::Enter)]));
    assert!(messages.iter().any(|m| matches!(m, Message::Session(_, session::Event::Send))), "{messages:?}");
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
            let messages = live.frame(&app, |ui| ui.send(vec![key(iced::keyboard::key::Named::Enter)]));
            assert!(messages.iter().any(|m| matches!(m, Message::Session(_, session::Event::Send))), "returning from {page:?}: {messages:?}");
        }
    }
}

/// Crossing the timer and its gap towards + must not collapse the timer and move the button.
#[test]
fn the_timer_stays_open_when_moving_towards_the_plus_button() {
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    app.daemon_settings = Some(silli_proto::admin::Settings { timer_auto: Some(false), ..Default::default() });
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
    assert!(!app.activity_hovered, "timer closes after leaving the controls");
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
            assert!((before - after.unwrap()).abs() < 1.0, "menu open={open}, at tail={at_tail}: {before} → {after:?}");
        }
    }
}

#[test]
fn folder_chips_show_tracked_lines_and_untracked_files_independently() {
    use silli_client::files::{Repo, Root, key};
    use silli_proto::sessions::HostPath;
    let (mut app, _) = App::new(true, None);
    let path = HostPath::from_bytes("/project".as_bytes());
    let view = app.views.get_mut("demo").unwrap();
    view.files.roots = vec![Root { path: path.clone(), global: false, metadata: None, error: None }];
    view.files
        .repos
        .insert(key(&path).into(), Repo { root: Some(path.clone()), lines: Some(("0".into(), "0".into())), summary: "+0 −0  ?3".into(), ..Repo::default() });
    {
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("?3").expect("new files remain visible with no tracked line changes");
        assert!(ui.find("+0").is_err());
        assert!(ui.find("−0").is_err());
    }
    let repo = app.views.get_mut("demo").unwrap().files.repos.get_mut(key(&path)).unwrap();
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
    let input = iced::widget::text_input("", "alpha beta gamma").id("edit").on_input(Message::Copy);
    let view = crate::keys::intercept_shortcuts(input);
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (600.0, 100.0), view);
    ui.click(iced::widget::Id::new("edit")).unwrap();
    let mut events = hotkey(Key::Character("2".into()), Modifiers::COMMAND);
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut events[1] {
        *text = Some("2".into()); // even backends supplying printable Ctrl text cannot insert it
    }
    ui.send(events);
    ui.send(hotkey(Key::Character("t".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character("T".into()), Modifiers::COMMAND | Modifiers::SHIFT));
    ui.send(hotkey(Key::Character("w".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character(",".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character(".".into()), Modifiers::COMMAND));
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(
        matches!(
            messages.as_slice(),
            [Message::SelectTab(1), Message::NewSession, Message::ToggleTerminal, Message::CloseCurrent, Message::StepTab(-1), Message::StepTab(1)]
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
    for binding in [|press| crate::ui::enter_sends("demo", press), crate::ui::reading_keys, Binding::<Message>::from_key_press] {
        for (key, motion) in [(Named::ArrowLeft, Motion::WordLeft), (Named::ArrowRight, Motion::WordRight)] {
            let event = |modifiers| KeyPress {
                key: Key::Named(key),
                modified_key: Key::Named(key),
                physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
                modifiers,
                text: None,
                status: Status::Focused { is_hovered: false },
            };
            assert!(matches!(binding(event(JUMP | Modifiers::SHIFT)), Some(Binding::Select(m)) if m == motion));
            assert!(matches!(binding(event(JUMP)), Some(Binding::Move(m)) if m == motion));
        }
    }
    let (mut app, _) = App::new(true, None);
    type_text(&mut app, "alpha beta gamma");
    let mut live = Live::new((720.0, 860.0));
    live.operations.push(Box::new(iced::advanced::widget::operation::focusable::focus(iced::widget::Id::new("prompt"))));
    let messages = live.frame(&app, |ui| {
        ui.send(hotkey(Key::Named(Named::Home), Modifiers::empty()));
        ui.send(hotkey(Key::Named(Named::ArrowRight), JUMP | Modifiers::SHIFT));
    });
    for message in messages {
        let _ = app.update(message);
    }
    assert_eq!(demo_view(&app).draft.selection().as_deref(), Some("alpha"));
    assert_eq!(app.selected.as_deref(), Some("demo"));
    for (direction, expected) in [(Named::ArrowRight, "alpha beta"), (Named::ArrowLeft, "alpha ")] {
        let messages = live.frame(&app, |ui| {
            ui.send(hotkey(Key::Named(direction), JUMP | Modifiers::SHIFT));
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
    let input = iced::widget::text_input("", "alpha beta gamma").id("edit").on_input(Message::Copy);
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (600.0, 100.0), crate::keys::intercept_shortcuts(input));
    ui.click(iced::widget::Id::new("edit")).unwrap();
    ui.send(hotkey(Key::Named(Named::Home), Modifiers::empty()));
    ui.send(hotkey(Key::Named(Named::ArrowRight), JUMP | Modifiers::SHIFT));
    let mut insert = hotkey(Key::Character("X".into()), Modifiers::empty());
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut insert[1] {
        *text = Some("X".into());
    }
    ui.send(insert);
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(matches!(messages.as_slice(), [Message::Copy(text)] if text == "X beta gamma"), "{messages:?}");
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
        let req = daemon.expect(method::SESSIONS_CREATE);
        assert_eq!(req["params"]["agent"], "codex");
        assert_eq!(req["params"]["remember_pins"], true);
        daemon.reply(&req, json!({"session_id":"fresh"}));
        let req = daemon.expect(method::SESSIONS_LIST);
        daemon.reply(&req, json!({"sessions":[session_record("fresh", "Fresh", false, false)], "next":null}));
        let req = daemon.expect(method::SESSIONS_END);
        assert_eq!(req["params"]["session_id"], "fresh");
        daemon.reply(&req, json!({}));
        let req = daemon.expect(method::SESSIONS_LIST);
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
    let kept = crate::settings::DisplayPrefs { shelf_width: Some(0.3), work_width: Some(0.6), ..Default::default() };
    app.settings.displays.insert("DP-1".into(), kept.clone());
    let _ = app.update(Message::ScaleStep(1));
    assert_eq!(app.settings.scale(), 1.1);
    let _ = app.update(Message::ScaleStep(1));
    assert_eq!(app.settings.scale(), 1.25);
    assert!(near(app.width, 400.0) && near(app.height, 640.0), "500 × 800 at 100 % in 125 % units");
    let monitor = app.monitor.unwrap();
    assert!(near(monitor.width, 1152.0) && near(monitor.height, 720.0), "{monitor:?}");
    assert_eq!(app.settings.displays["DP-1"], kept);
    let _ = app.update(Message::ScaleStep(0));
    assert!(near(app.width, 500.0) && near(app.height, 800.0));
    assert_eq!(app.settings.displays["DP-1"], kept);
}

#[test]
fn configured_shortcuts_intercept_inputs_and_release_old_bindings() {
    use iced::keyboard::{Key, Modifiers};
    let bindings = crate::shortcuts::Bindings::from([("new_session".into(), "Alt+N".into()), ("close_session".into(), String::new())]);
    let input = iced::widget::text_input("", "").id("custom-edit").on_input(Message::Copy);
    let view = crate::keys::configured_shortcuts(input, &bindings);
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (600.0, 100.0), view);
    ui.click(iced::widget::Id::new("custom-edit")).unwrap();
    ui.send(hotkey(Key::Character("n".into()), Modifiers::ALT));
    ui.send(hotkey(Key::Character("t".into()), Modifiers::COMMAND));
    ui.send(hotkey(Key::Character("w".into()), Modifiers::COMMAND));
    let messages: Vec<_> = ui.into_messages().collect();
    assert_eq!(messages.iter().filter(|m| matches!(m, Message::NewSession)).count(), 1);
    assert!(!messages.iter().any(|m| matches!(m, Message::CloseCurrent)));
    assert!(crate::keys::from_text(&Key::Character("t".into()), Modifiers::COMMAND, true).is_none());
}

/// Settings records a shortcut from the keys pressed: while it does, the press reaches no
/// window shortcut and no field, and what is stored is the physical key.
#[test]
fn a_shortcut_is_recorded_from_a_key_press() {
    use crate::settings_ui::Event;
    use iced::keyboard::key::{Code, Named, Physical};
    use iced::keyboard::{Key, Modifiers};
    let press = |key: Key, code: Code, modifiers: Modifiers| {
        let mut events = hotkey(key, modifiers);
        if let Some(iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { physical_key, .. })) = events.last_mut() {
            *physical_key = Physical::Code(code);
        }
        events
    };
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::ShowPage(crate::Page::Settings));
    let page = app.settings_page.as_mut().unwrap();
    page.snapshot = Some(Ok(silli_client::settings::Snapshot::default()));
    page.category = crate::settings_ui::Category::Interface;
    page.collapsed.remove("Keyboard shortcuts");
    let _ = app.update(Message::Settings(Event::RecordShortcut("new_session")));
    assert_eq!(app.settings_page.as_ref().unwrap().recording, Some("new_session"));

    // the window's own Ctrl+W is not acted on while recording: it goes to the recorder
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (900.0, 5000.0), app.view());
    ui.find("Press keys…").unwrap();
    ui.send(press(Key::Character("w".into()), Code::KeyW, Modifiers::COMMAND));
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(matches!(messages.as_slice(), [Message::Settings(Event::ShortcutKey(..))]), "{messages:?}");
    for message in messages {
        let _ = app.update(message);
    }
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!(page.recording, Some("new_session"), "a refused chord leaves the field recording");
    assert!(page.shortcut_refused.as_ref().unwrap().contains("conflicts"));
    assert!(app.settings.shortcuts.is_empty());

    // a modifier alone is not a chord; the key with it is, by its place on the keyboard
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Named(Named::Alt), Physical::Code(Code::AltLeft), Modifiers::ALT)));
    assert_eq!(app.settings_page.as_ref().unwrap().recording, Some("new_session"));
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Character("n".into()), Physical::Code(Code::KeyN), Modifiers::ALT)));
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!((page.recording, page.shortcut_refused.as_deref()), (None, None));
    assert_eq!(app.settings.shortcuts.get("new_session").map(String::as_str), Some("Alt+N @KeyN"));
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (900.0, 700.0), app.view());
    ui.send(press(Key::Character("˜".into()), Code::KeyN, Modifiers::ALT));
    assert!(ui.into_messages().any(|m| matches!(m, Message::NewSession)));

    // Escape leaves the binding, Backspace disables it, another request of the page stops recording
    let _ = app.update(Message::Settings(Event::RecordShortcut("new_session")));
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::empty())));
    assert_eq!(app.settings_page.as_ref().unwrap().recording, None);
    assert_eq!(app.settings.shortcuts.get("new_session").map(String::as_str), Some("Alt+N @KeyN"));
    let _ = app.update(Message::Settings(Event::RecordShortcut("new_session")));
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Named(Named::Backspace), Physical::Code(Code::Backspace), Modifiers::empty())));
    assert_eq!(app.settings.shortcuts.get("new_session").map(String::as_str), Some(""));
    let _ = app.update(Message::Settings(Event::RecordShortcut("close_session")));
    let _ = app.update(Message::Settings(Event::ToggleSection("Keyboard shortcuts")));
    assert_eq!(app.settings_page.as_ref().unwrap().recording, None);
}

/// The global hotkey is recorded as the shortcuts are, into silli-gui's own settings; while it
/// is recorded in a shown window no key is held, so that the press reaches the recorder.
#[test]
fn the_hotkey_is_recorded_and_let_go_meanwhile() {
    use crate::settings_ui::{Event, HOTKEY};
    use iced::keyboard::key::{Code, Named, Physical};
    use iced::keyboard::{Key, Modifiers};
    let (mut app, _) = App::new(true, None);
    app.visible = true;
    let _ = app.update(Message::ShowPage(crate::Page::Settings));
    assert_eq!(app.hotkey_wanted(), crate::hotkey::DEFAULT);
    let _ = app.update(Message::Settings(Event::RecordShortcut(HOTKEY)));
    assert_eq!(app.hotkey_wanted(), "");
    // a hidden window records nothing: the key is held again
    app.visible = false;
    assert_eq!(app.hotkey_wanted(), crate::hotkey::DEFAULT);
    app.visible = true;

    // a key that would be swallowed everywhere is refused, and the field goes on recording
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Character("a".into()), Physical::Code(Code::KeyA), Modifiers::empty())));
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!(page.recording, Some(HOTKEY));
    assert!(page.shortcut_refused.is_some());
    assert_eq!(app.settings.hotkey, None);

    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Character("`".into()), Physical::Code(Code::Backquote), Modifiers::LOGO)));
    assert_eq!(app.settings_page.as_ref().unwrap().recording, None);
    // the logo key by this system's name for it
    let recorded = if cfg!(target_os = "macos") {
        "Cmd+` @Backquote"
    } else if cfg!(windows) {
        "Win+` @Backquote"
    } else {
        "Super+` @Backquote"
    };
    assert_eq!(app.settings.hotkey.as_deref(), Some(recorded));
    assert_eq!(app.hotkey_wanted(), recorded);
    // a window shortcut may share nothing with it: they are apart
    assert!(app.settings.shortcuts.is_empty());

    // Escape leaves it, Backspace sets none, Reset brings the default back
    let _ = app.update(Message::Settings(Event::RecordShortcut(HOTKEY)));
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::empty())));
    assert_eq!(app.hotkey_wanted(), recorded);
    let _ = app.update(Message::Settings(Event::RecordShortcut(HOTKEY)));
    let _ = app.update(Message::Settings(Event::ShortcutKey(Key::Named(Named::Backspace), Physical::Code(Code::Backspace), Modifiers::empty())));
    assert_eq!(app.settings.hotkey.as_deref(), Some(""));
    assert_eq!(app.hotkey_wanted(), "");
    let _ = app.update(Message::Settings(Event::ResetHotkey));
    assert_eq!(app.hotkey_wanted(), crate::hotkey::DEFAULT);
    // leaving the page while it records ends the recording
    let _ = app.update(Message::Settings(Event::RecordShortcut(HOTKEY)));
    let _ = app.update(Message::ShowPage(crate::Page::Changes));
    assert_eq!(app.hotkey_wanted(), crate::hotkey::DEFAULT);
}

#[test]
fn policy_errors_remain_visible_with_the_group_collapsed() {
    let (mut app, _) = App::new(true, None);
    let mut page = crate::settings_ui::SettingsPage {
        snapshot: Some(Ok(silli_client::settings::Snapshot::default())),
        category: crate::settings_ui::Category::AgentBehaviour,
        ..Default::default()
    };
    page.policies.status = "Policy validation failed: invalid TOML".into();
    let view = app.settings_view(&page);
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 5000.0), view);
    ui.find("Policies").unwrap();
    ui.find("Policy validation failed: invalid TOML").unwrap();
    assert!(ui.find("Import policy").is_err());
    drop(ui);
    page.collapsed.remove("Policies");
    app.settings_page = Some(page);
    let view = app.settings_view(app.settings_page.as_ref().unwrap());
    let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 5000.0), view);
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
    let _ = app.update(Message::Terminal(id, jail_generation, crate::terminal::Event::Ended));
    assert_eq!(app.current().unwrap().parked_terminal.as_ref().unwrap().0.status, crate::terminal::Status::Ended);
    let _ = app.update(Message::ToggleHostTerminal);
    assert!(!app.current().unwrap().terminal_shown);
    let _ = app.update(Message::ToggleHostTerminal);
    assert_eq!(app.current().unwrap().terminal_generation, host_generation);
    let _ = app.update(Message::ToggleTerminal);
    assert!(!app.current().unwrap().terminal_host);
    assert!(app.current().unwrap().terminal_generation > host_generation, "ended jail shell is reopened without reusing host generation");
    let _ = app.update(Message::ToggleHostTerminal);
    assert_eq!(app.current().unwrap().terminal_generation, host_generation);
}

#[test]
fn host_shell_shortcut_is_configurable() {
    use iced::keyboard::{Key, Modifiers};
    let mut bindings = crate::shortcuts::Bindings::new();
    let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
    assert!(matches!(crate::shortcuts::message(&bindings, &Key::Character("h".into()), modifiers), Some(Message::ToggleHostTerminal)));
    bindings.insert("host_shell".into(), "Alt+H".into());
    assert!(crate::shortcuts::message(&bindings, &Key::Character("h".into()), modifiers).is_none());
    assert!(matches!(crate::shortcuts::message(&bindings, &Key::Character("h".into()), Modifiers::ALT), Some(Message::ToggleHostTerminal)));
}

#[test]
fn attachment_only_send_keeps_files_on_refusal() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let attachment = silli_proto::sfp::Attachment::Uploaded { upload_id: "document".into(), name: "report.pdf".into(), mime: "application/pdf".into() };
    app.views.get_mut("demo").unwrap().attachments.push(attachment.clone().into());
    let task = app.update(in_session(&app, session::Event::Send));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    let submit: Submit = serde_json::from_value(request["params"].clone()).unwrap();
    assert!(submit.text.is_empty());
    assert_eq!(submit.attachments, vec![attachment.clone()]);
    daemon.reply(&request, json!({}));
    settle_task(&mut app, task);
    assert!(demo_view(&app).attachments.is_empty());
    let view = app.views.get_mut("demo").unwrap();
    let submit = view.outbox.send_with_attachments("demo", String::new(), vec![attachment.clone()]);
    let _ = app.update(Message::Done("demo".into(), crate::Act::Send(submit.submission_id), silli_client::actions::Outcome::NotSent("offline".into())));
    assert_eq!(demo_view(&app).attachments.iter().map(|a| a.attachment.clone()).collect::<Vec<_>>(), vec![attachment]);
}

#[test]
fn attachment_completion_stays_with_originating_session() {
    let (mut app, _) = App::new(true, None);
    add_session(&mut app, "other");
    app.selected = Some("other".into());
    let a = silli_proto::sfp::Attachment::Uploaded { upload_id: "file".into(), name: "notes.txt".into(), mime: "text/plain".into() };
    let _ = app.update(Message::Media(crate::media::Event::Attached("demo".into(), Ok(vec![(a.clone(), None)]))));
    assert_eq!(app.views["demo"].attachments.iter().map(|a| a.attachment.clone()).collect::<Vec<_>>(), vec![a]);
    assert!(app.views["other"].attachments.is_empty());
}

#[test]
fn generic_attachment_upload_uses_session_and_preserves_file_bytes() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let path = std::env::temp_dir().join(format!("silli-attachment-{}.txt", silli_client::request_id("test")));
    let bytes = b"notes\n\0binary-safe";
    std::fs::write(&path, bytes).unwrap();
    let task = app.attachment_paths("demo".into(), Some(vec![path.clone()]));
    let request = daemon.expect(method::UPLOADS_CREATE);
    let params: silli_proto::sfp::UploadCreate = serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(params.session_id, "demo");
    assert_eq!(silli_proto::admin::decode_bytes(&params.data_base64).unwrap(), bytes);
    daemon.reply(&request, json!({"upload_id":params.upload_id}));
    settle_task(&mut app, task);
    assert!(matches!(&demo_view(&app).attachments[0].attachment, silli_proto::sfp::Attachment::Uploaded { upload_id, .. } if upload_id == &params.upload_id));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn markdown_image_is_read_from_session_host_and_decoded() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let source = "![result](/tmp/image%20one.png)";
    app.views.get_mut("demo").unwrap().rendered.insert(
        "test-image".into(),
        crate::Rendered { source: source.into(), markdown: iced::widget::markdown::Content::parse(source), leaves: std::sync::OnceLock::new() },
    );
    let task = app.load_media("demo");
    let request = daemon.expect(method::FILES_IMAGE);
    let params: silli_proto::admin::SessionImage = serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(params.session_id, "demo");
    assert_eq!(params.path.display, "/tmp/image one.png");
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
    daemon.reply(
        &request,
        json!({"bytes_base64":silli_proto::admin::encode_bytes(png.get_ref()),"next_offset":png.get_ref().len().to_string(),"eof":true,"revision":"one"}),
    );
    settle_task(&mut app, task);
    assert!(demo_view(&app).media.values().any(|v| matches!(v, Some(Ok(_)))));
}

#[test]
fn new_tabs_append_and_refreshes_preserve_existing_positions() {
    let (mut app, _) = App::new(true, None);
    app.tabs.clear();
    let mut older = session_record("older", "Older", false, false);
    older.created_at = "9".into();
    let mut newer = session_record("newer", "Newer", false, false);
    newer.created_at = "10".into();
    let ids = |app: &App| app.tabs.iter().map(|t| t.id.as_str().to_owned()).collect::<Vec<_>>();
    let _ = app.update(Message::Listed(Ok(vec![newer.clone(), older.clone()])));
    assert_eq!(ids(&app), ["older", "newer"]);
    let mut newest = session_record("newest", "Newest", false, false);
    newest.created_at = "11".into();
    let _ = app.update(Message::Listed(Ok(vec![newest.clone(), newer.clone(), older.clone()])));
    assert_eq!(ids(&app), ["older", "newer", "newest"]);
    let _ = app.update(Message::Listed(Ok(vec![older.clone(), newest.clone(), newer.clone()])));
    assert_eq!(ids(&app), ["older", "newer", "newest"]);
    older.ended_at = Some("12".into());
    let _ = app.update(Message::Listed(Ok(vec![older.clone(), newest.clone(), newer.clone()])));
    older.ended_at = None;
    let _ = app.update(Message::Listed(Ok(vec![older, newest, newer])));
    assert_eq!(ids(&app), ["newer", "newest", "older"], "reopened tabs append too");
}

#[test]
fn double_clicking_a_tab_title_starts_name_editing_without_dragging_window() {
    let (app, _) = App::new(true, None);
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.click("witty-willow").unwrap();
    ui.click("witty-willow").unwrap();
    let messages: Vec<_> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::Name(crate::names::Event::Begin(id)) if id == "demo")), "{messages:?}");
    assert!(!messages.iter().any(|m| matches!(m, Message::DragWindow)), "{messages:?}");
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
    let _ = app.name_event(Event::Done(token, "demo".into(), "my-session".into(), Ok(())));
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
            if request["method"] == method::POLICY_PREVIEW {
                daemon.refuse(&request, "unavailable", "not in this test");
            } else {
                requests.push(request);
            }
        }
        started.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        for request in requests {
            match request["method"].as_str().unwrap() {
                method::SESSIONS_LIST => daemon.reply(&request, json!({"sessions": [session], "next": null})),
                method::SETTINGS_GET => daemon.reply(&request, silli_proto::admin::Settings::default()),
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
    assert!(!app.list_again, "timer ticks must not perpetually invalidate slow reads");
    release.send(()).unwrap();
    settle_task(&mut app, task);
    fake.join().unwrap();
    assert!(!app.list_pending && !app.settings_pending);
    assert_eq!(app.tabs.len(), 1);
    let _ = app.update(Message::Listed(Err("offline".into())));
    app.settings_pending = true;
    let _ = app.update(Message::DaemonSettings(app.settings_revision, Err("offline".into())));
    assert!(!app.list_pending && !app.settings_pending, "failed reads release their guards too");
}

#[test]
fn explicit_relist_skips_obsolete_results_and_preserves_the_view() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    type_text(&mut app, "keep this draft");
    let session = session_record("demo", "reopened", false, false);
    let fake = std::thread::spawn(move || {
        let old = daemon.expect(method::SESSIONS_LIST);
        daemon.reply(&old, json!({"sessions": [], "next": null}));
        let fresh = daemon.expect(method::SESSIONS_LIST);
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
    let _ = app.settings_event(Event::Instructions(iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(Arc::new(
        "new instructions".into(),
    )))));
    let before = app.settings_page.as_ref().unwrap().instructions.text();
    let task = app.settings_event(Event::SaveInstructions);
    assert!(app.settings_page.as_ref().unwrap().edited, "no success before the reply");
    assert!(app.settings_page.as_ref().unwrap().instructions_saving.is_some());
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
    let stale = silli_client::settings::Snapshot { instructions: "old page's text".into(), ..Default::default() };
    let _ = app.update(Message::SettingsReply(old, Event::Loaded(String::new(), Box::new(Ok(stale)))));
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
    let _ = app.settings_event(Event::InstructionsSaved("saved text".into(), silli_client::actions::Outcome::Done(json!({}))));
    let _ = app.settings_event(Event::Loaded(
        app.settings_page.as_ref().unwrap().load_token.clone(),
        Box::new(Ok(silli_client::settings::Snapshot { instructions: "saved text".into(), ..Default::default() })),
    ));
    let page = app.settings_page.as_ref().unwrap();
    assert_eq!(page.instructions.text(), "newer text");
    assert!(page.edited);
    let _ = app.settings_event(Event::Loaded(
        app.settings_page.as_ref().unwrap().load_token.clone(),
        Box::new(Ok(silli_client::settings::Snapshot { instructions: "newer text".into(), ..Default::default() })),
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
        Event::Loaded(old_load, Box::new(Ok(silli_client::settings::Snapshot { instructions: "stale server value".into(), ..Default::default() }))),
    ));
    assert_eq!(app.settings_page.as_ref().unwrap().instructions.text(), before);
}

#[test]
fn background_settings_cannot_undo_a_newer_page_snapshot() {
    let (mut app, _) = App::new(true, None);
    let old_revision = app.settings_revision;
    let task = app.show_page(crate::Page::Settings);
    settle_task(&mut app, task);
    let before = app.daemon_settings.clone().unwrap();
    let _ = app.update(Message::DaemonSettings(old_revision, Ok(silli_proto::admin::Settings { theme: "stale-theme".into(), ..Default::default() })));
    assert_eq!(app.daemon_settings.as_ref().unwrap().theme, before.theme);
}

#[test]
fn prompt_undo_and_redo_shortcuts() {
    use iced::keyboard::{Key, Modifiers, key};
    use iced::widget::text_editor::{Binding, KeyPress, Status};
    for (character, modifiers, redo) in [("z", Modifiers::COMMAND, false), ("z", Modifiers::COMMAND | Modifiers::SHIFT, true), ("y", Modifiers::COMMAND, true)]
    {
        let press = KeyPress {
            key: Key::Character(character.into()),
            modified_key: Key::Character(character.into()),
            physical_key: key::Physical::Code(key::Code::KeyZ),
            modifiers,
            text: None,
            status: Status::Focused { is_hovered: false },
        };
        assert!(matches!(crate::ui::enter_sends("demo", press.clone()),
            Some(Binding::Custom(Message::Session(_, session::Event::PromptHistory(value)))) if value == redo));
        assert!(matches!(crate::ui::reading_keys(press),
            Some(Binding::Custom(Message::Files(crate::files::Action::History(value)))) if value == redo));
    }
}

#[test]
fn prompt_list_keys() {
    use iced::keyboard::{Key, Modifiers, key};
    use iced::widget::text_editor::{Binding, KeyPress, Status};
    let press = |named, modifiers, focused| KeyPress {
        key: Key::Named(named),
        modified_key: Key::Named(named),
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        modifiers,
        text: None,
        status: if focused { Status::Focused { is_hovered: false } } else { Status::Active },
    };
    let (plain, shift) = (Modifiers::empty(), Modifiers::SHIFT);
    use key::Named::{Enter, Tab};
    assert!(matches!(crate::ui::enter_sends("demo", press(Enter, plain, true)), Some(Binding::Custom(Message::Session(_, session::Event::Send)))));
    assert!(matches!(crate::ui::enter_sends("demo", press(Enter, shift, true)), Some(Binding::Custom(Message::Session(_, session::Event::PromptNewline)))));
    assert!(matches!(crate::ui::enter_sends("demo", press(Tab, plain, true)), Some(Binding::Custom(Message::Session(_, session::Event::PromptIndent(false))))));
    assert!(matches!(crate::ui::enter_sends("demo", press(Tab, shift, true)), Some(Binding::Custom(Message::Session(_, session::Event::PromptIndent(true))))));
    // an unfocused prompt and Ctrl+Tab leave Tab to the window
    assert!(crate::ui::enter_sends("demo", press(Tab, plain, false)).is_none());
    assert!(crate::ui::enter_sends("demo", press(Tab, Modifiers::COMMAND, true)).is_none());
    // on a plain line Tab changes nothing in the draft; in a list it indents, undone in one step
    let (mut app, _) = App::new(true, None);
    for (draft, indented) in [("plain", "plain"), ("- item", "  - item")] {
        let view = app.current_mut().unwrap();
        view.draft = crate::editor::Editor::with_text(draft);
        let _ = app.update(in_session(&app, session::Event::PromptIndent(false)));
        assert_eq!(app.current().unwrap().draft.text(), indented);
        let _ = app.update(in_session(&app, session::Event::PromptNewline));
        assert_ne!(app.current().unwrap().draft.text(), indented);
        let _ = app.update(in_session(&app, session::Event::PromptHistory(false)));
        assert_eq!(app.current().unwrap().draft.text(), indented);
    }
    // where Enter does not send, it is the list's line break
    assert!(matches!(crate::ui::list_keys(&press(Enter, plain, true), false), Some(crate::ui::ListKey::Newline)));
    assert!(crate::ui::list_keys(&press(Enter, shift, true), false).is_none());
}

/// The window's geometry through the app (`geometry.rs`): Silli's own placement is not taken for
/// the person's; the person's resize is kept for the display it is on; a display change puts the
/// window where it belongs without keeping what the window manager did meanwhile; Shelf mode
/// keeps only its width, for the main display.
#[test]
fn the_window_keeps_what_the_person_made_of_it_per_display() {
    use crate::geometry::{Controller, Display, Rect, target};
    use crate::layout::Mode;
    let rect = |x, y, width, height| Rect { x, y, width, height };
    let laptop = Display { name: "eDP-1".into(), primary: true, bounds: rect(0, 0, 1920, 1080), work: rect(0, 0, 1920, 1040) };
    let external = Display { name: "DP-1".into(), primary: false, bounds: rect(1920, 0, 2560, 1440), work: rect(1920, 0, 2560, 1440) };
    let both = vec![laptop.clone(), external];
    let poll = |app: &mut App, displays: &[Display], window: Rect| {
        let read = Some((displays.to_vec(), Some(window)));
        let _ = app.update(Message::Geometry(crate::Read::Poll, read));
    };
    let (mut app, _) = App::new(true, None);
    app.geometry = Controller::immediate();
    app.visible = true;
    app.layout.mode = Mode::Drawer;
    // placed by Silli: half the main display, its usable height
    let placed = target(Mode::Drawer, &app.settings, &both).unwrap();
    assert_eq!(placed.rect, rect(0, 0, 960, 1040));
    let _ = app.update(Message::Placed(Some((both.clone(), Some(placed.clone())))));
    poll(&mut app, &both, placed.rect);
    poll(&mut app, &both, placed.rect);
    assert!(app.settings.displays.is_empty(), "Silli's own placement is not kept");
    // the person drags it onto the external display and sizes it there
    let dragged = rect(2560, 360, 1280, 720);
    poll(&mut app, &both, dragged);
    poll(&mut app, &both, dragged);
    let kept = &app.settings.displays["DP-1"];
    assert_eq!((kept.work_width, kept.work_height, kept.work_x, kept.work_y), (Some(0.5), Some(0.5), Some(0.25), Some(0.25)));
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
    let narrower = Rect { width: 576, ..shelf.rect };
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
    use crate::geometry::{Controller, Display, Rect, target};
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
    assert!(app.settings.displays.is_empty(), "Silli's own placement is not kept");
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
    let source = silli_proto::sessions::HostPath::from_bytes("/home/demo/project/README.md".as_bytes());
    let _ = app.update(Message::Files(crate::files::Action::Open { path: source.clone(), repo: None }));
    let view = app.views.get_mut("demo").unwrap();
    let file = view.files.reader.as_mut().unwrap();
    file.text = "A [document link](docs/next.md#heading).".into();
    file.revision += 1;
    view.sync_reader();
    let mut ui = simulator(&app, (1000.0, 860.0));
    let at = aim_at(&mut ui, "A document link", "document");
    ui.aim(at);
    ui.send(vec![iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Right))]);
    assert_eq!(copies(ui.into_messages()), ["docs/next.md#heading"]);
    let mut ui = simulator(&app, (1000.0, 860.0));
    let at = aim_at(&mut ui, "A document link", "document");
    ui.aim(at);
    ui.send(vec![
        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
    ]);
    assert!(ui.into_messages().any(|m| matches!(m, Message::Files(crate::files::Action::Link { session, source: from, target })
        if session == "demo" && from.display == source.display && target == "docs/next.md#heading")));
}

#[test]
fn closing_tabs_returns_to_the_most_recent_open_tab() {
    let (mut app, _) = App::new(true, None);
    let records =
        || vec![session_record("demo", "Demo", false, false), session_record("middle", "Middle", false, false), session_record("last", "Last", false, false)];
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
    Message::Lane(id.into(), view.lane_epoch, silli_client::lane::Update::Event(Box::new(event)))
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
        Message::Lane(id.into(), view.lane_epoch, silli_client::lane::Update::Event(Box::new(event)))
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
    view.status = silli_client::lane::Status::Live;
    view.conversation.session.as_mut().unwrap().controls = None;
    simulator(&app, (1200.0, 860.0)).find("Loading controls…").unwrap();
    let controls = SessionControls {
        status: "restoring".into(),
        error: None,
        current_mode: Some("saved".into()),
        modes: vec![ControlChoice { value: "saved".into(), title: "Saved selection".into() }],
        options: vec![],
    };
    app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls = Some(controls);
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        ui.find("Refreshing controls…").unwrap();
        ui.find("Saved selection").unwrap();
        ui.click("Saved selection").unwrap();
        assert!(
            !ui.into_messages()
                .any(|message| matches!(message, Message::Session(_, session::Event::SetMode(_)) | Message::Session(_, session::Event::SetOption(..))))
        );
    }
    app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap().status = "ready".into();
    app.views.get_mut("demo").unwrap().status = silli_client::lane::Status::Reconnecting("offline".into());
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        ui.find("Refreshing controls…").unwrap();
        ui.click("Saved selection").unwrap();
        assert!(!ui.into_messages().any(|message| matches!(message, Message::Session(_, session::Event::SetMode(_)))));
    }
    app.views.get_mut("demo").unwrap().status = silli_client::lane::Status::Live;
    let controls = app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap();
    controls.status = "ready".into();
    controls.current_mode = None;
    controls.modes.clear();
    {
        let mut ui = simulator(&app, (1200.0, 860.0));
        assert!(ui.find("Loading controls…").is_err());
        assert!(ui.find("Refreshing controls…").is_err());
    }
    let controls = app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap().controls.as_mut().unwrap();
    controls.status = "failed".into();
    controls.error = Some("Agent did not confirm effort".into());
    assert!(simulator(&app, (1200.0, 860.0)).find("Settings warning").is_err());
}

/// The modifier that moves by words, as iced's editors take it (`Modifiers::jump`): Alt (Option)
/// on macOS, Ctrl elsewhere.
const JUMP: iced::keyboard::Modifiers = if cfg!(target_os = "macos") { iced::keyboard::Modifiers::ALT } else { iced::keyboard::Modifiers::CTRL };

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
    assert!(matches!(messages.as_slice(), [Message::Session(_, session::Event::ToggleTool(id))] if id == "raw"));
}

/// A session the daemon could not read (a launch recorded by an older Silli) is a note of its own:
/// the other sessions' entries are listed, and the page shows no error over them.
#[test]
fn an_unreadable_session_is_a_note_not_an_error_over_the_listing() {
    use crate::changes_ui::Event;
    let (mut app, _) = App::new(true, None);
    let task = app.update(Message::ToggleChanges);
    settle_task(&mut app, task);
    let unreadable = vec![silli_client::changes::Unreadable {
        session_id: "bf590a53".into(),
        title: "Repo review".into(),
        why: "read launch policy snapshot: missing field `kind`".into(),
    }];
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), unreadable.clone(), 0)))));
    let listing = &app.changes_listing;
    assert_eq!(listing.entries.as_ref().unwrap().as_ref().unwrap().len(), demo::changes().len());
    assert_eq!(listing.unreadable, unreadable);
    let page = app.changes.as_ref().unwrap();
    let _ = app.changes_view(page);
    // the next listing without it clears the note
    let _ = app.update(Message::Changes(Event::Loaded(Ok((demo::changes(), vec![], 0)))));
    assert!(app.changes_listing.unreadable.is_empty());
}

// --- find (Ctrl+F) ------------------------------------------------------------------------------

fn find_matched(ui: &mut impl Drive) -> Vec<String> {
    let found = Arc::new(Mutex::new(Vec::new()));
    let sink = found.clone();
    let _: Option<()> = ui.select(move |candidate| {
        if let iced_test::selector::Candidate::Custom { state, .. } = candidate
            && let Some(text) = state.downcast_ref::<crate::selection::Selectable>()
        {
            for part in text.find_matched_texts() {
                sink.lock().unwrap().push(part.to_string());
            }
        }
        None
    });
    found.lock().unwrap().clone()
}

fn current_matched(ui: &mut impl Drive) -> Vec<(String, iced::Rectangle)> {
    let found = Arc::new(Mutex::new(Vec::new()));
    let sink = found.clone();
    let _: Option<()> = ui.select(move |candidate| {
        if let iced_test::selector::Candidate::Custom { state, .. } = candidate
            && let Some(text) = state.downcast_ref::<crate::selection::Selectable>()
            && let Some(part) = text.current_matched_text()
            && let Some(rect) = text.current_match_rect()
        {
            sink.lock().unwrap().push((part.to_string(), rect));
        }
        None
    });
    found.lock().unwrap().clone()
}

fn ctrl_f() -> Vec<iced::Event> {
    use iced::keyboard::{Event, Key, Location, Modifiers, key};
    vec![
        iced::Event::Keyboard(Event::ModifiersChanged(Modifiers::COMMAND)),
        iced::Event::Keyboard(Event::KeyPressed {
            key: Key::Character("f".into()),
            modified_key: Key::Character("f".into()),
            physical_key: key::Physical::Code(key::Code::KeyF),
            location: Location::Standard,
            modifiers: Modifiers::COMMAND,
            text: None,
            repeat: false,
        }),
        iced::Event::Keyboard(Event::ModifiersChanged(Modifiers::empty())),
    ]
}

#[test]
fn find_opens_from_ctrl_f_prepopulates_selection_and_steps_matches() {
    use crate::find::Event as FindEvent;
    let mut app = streamed();
    let mut live = Live::new((720.0, 860.0));
    let events = live.frame(&app, |ui| {
        let from = aim_at(ui, "Make the send arrow", "arrow");
        let to = aim_at(ui, "Make the send arrow", " a little");
        drag(ui, from, to);
        ui.send(ctrl_f());
    });
    assert!(events.iter().any(|m| matches!(m, Message::Find(FindEvent::Open))), "{events:?}");
    for m in events {
        live.apply(&mut app, m);
    }
    assert!(app.find.shown);
    assert_eq!(app.find.query, "arrow");
    assert_eq!(app.find.matches.len(), 4);
    assert_eq!(app.find.current, Some(0));
    assert_eq!(app.find.count_label(), Some(("1 of 4".to_string(), true)));

    let mut matched = Vec::new();
    let mut current = Vec::new();
    live.frame(&app, |ui| {
        matched = find_matched(ui);
        current = current_matched(ui);
    });
    assert_eq!(matched, ["arrow", "arrow", "arrow", "arrow"]);
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "arrow");
    assert!(current[0].1.width > 0.0 && current[0].1.height > 0.0);

    // Step forward through all matches and wrap around to 0.
    for expected in [1, 2, 3, 0] {
        live.apply(&mut app, Message::Find(FindEvent::Next));
        assert_eq!(app.find.current, Some(expected));
    }
    // Step backward and wrap around to the last match.
    live.apply(&mut app, Message::Find(FindEvent::Prev));
    assert_eq!(app.find.current, Some(3));

    // Case sensitivity toggle ("Aa" / Alt+C).
    live.apply(&mut app, Message::Find(FindEvent::Input("silli".into())));
    assert_eq!(app.find.matches.len(), 3); // 2x "silli-gui" (tool command + detail) + 1x "Silli GUI"
    live.apply(&mut app, Message::Find(FindEvent::Next));
    assert_eq!(app.find.current, Some(1));
    // Pressing Ctrl+F again while Find is already open keeps the edited query and current match
    // (even though "arrow" is still selected in the conversation).
    live.apply(&mut app, Message::Find(FindEvent::Open));
    assert_eq!(app.find.query, "silli");
    assert_eq!(app.find.current, Some(1));

    live.apply(&mut app, Message::Find(FindEvent::ToggleCase));
    assert!(app.find.case_sensitive);
    assert_eq!(app.find.matches.len(), 2); // only lowercase "silli" in "silli-gui"
    live.apply(&mut app, Message::Find(FindEvent::Input("Silli".into())));
    assert_eq!(app.find.matches.len(), 1); // only uppercase "Silli" in "Silli GUI"
    live.apply(&mut app, Message::Find(FindEvent::ToggleCase));
    assert!(!app.find.case_sensitive);
    assert_eq!(app.find.matches.len(), 3);

    // Non-matching query reports "No matches".
    live.apply(&mut app, Message::Find(FindEvent::Input("nonexistent-token".into())));
    assert!(app.find.matches.is_empty());
    assert_eq!(app.find.current, None);
    assert_eq!(app.find.count_label(), Some(("No matches".to_string(), false)));
}

#[test]
fn find_searches_and_unfolds_folded_content_in_conversation() {
    use crate::find::Event as FindEvent;
    let (mut app, _) = App::new(true, None);
    let view = app.views.get_mut("demo").unwrap();
    view.conversation = silli_client::Conversation::new("demo", "sub", 1);
    view.conversation.session = demo::conversation().session;

    // 1. A folded turn (summary turn)
    view.conversation.submissions.insert(
        "fold-sub".into(),
        Submission {
            submission_id: "fold-sub".into(),
            session_id: "demo".into(),
            accepted_seq: "1".into(),
            agent_generation_id: "g".into(),
            text: "Summarize the session".into(),
            attachments: vec![],
            acceptance: "accepted".into(),
            delivery: "sent".into(),
            held: false,
            hold_reason: None,
            insertion: "inserted".into(),
            outcome: "completed".into(),
            reason: None,
            purpose: Some("summary".into()),
        },
    );
    view.conversation.turns.insert(
        "fold-turn".into(),
        Turn {
            turn_id: "fold-turn".into(),
            agent_generation_id: "g".into(),
            activity: "idle".into(),
            outcome: "completed".into(),
            reason: None,
            purpose: Some("summary".into()),
        },
    );
    view.conversation.insert_item(Item {
        item_id: "fold-u".into(),
        created_seq: "1".into(),
        turn_id: Some("fold-turn".into()),
        submission_id: Some("fold-sub".into()),
        agent_generation_id: "g".into(),
        kind: "user".into(),
        content: vec![Content::text("Fix the crosscheck issue")],
        tool: None,
    });
    view.conversation.insert_item(Item {
        item_id: "fold-a".into(),
        created_seq: "2".into(),
        turn_id: Some("fold-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "agent".into(),
        content: vec![Content::text("Applied folded-turn-needle cleanly.")],
        tool: None,
    });

    // 2. A grouped tool run (two rg calls in a normal turn)
    view.conversation.turns.insert(
        "main-turn".into(),
        Turn {
            turn_id: "main-turn".into(),
            agent_generation_id: "g".into(),
            activity: "idle".into(),
            outcome: "completed".into(),
            reason: None,
            purpose: None,
        },
    );
    view.conversation.insert_item(Item {
        item_id: "tool-1".into(),
        created_seq: "3".into(),
        turn_id: Some("main-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "tool".into(),
        content: vec![],
        tool: Some(Tool {
            title: "rg first-arg crates".into(),
            command: Some("rg first-arg crates".into()),
            kind: "execute".into(),
            status: "completed".into(),
            detail: "first output".into(),
            diff: String::new(),
            parent_item_id: None,
        }),
    });
    view.conversation.insert_item(Item {
        item_id: "tool-2".into(),
        created_seq: "4".into(),
        turn_id: Some("main-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "tool".into(),
        content: vec![],
        tool: Some(Tool {
            title: "rg grouped-call-needle crates".into(),
            command: Some("rg grouped-call-needle crates".into()),
            kind: "execute".into(),
            status: "completed".into(),
            detail: "found closed-output-needle in src/find.rs".into(),
            diff: "--- a/src/find.rs\n+++ b/src/find.rs\n+diff-line-needle\n".into(),
            parent_item_id: None,
        }),
    });

    // 3. A long thought (folded to 1 line by default)
    let long_thought = "First line of reasoning.\n\nSecond paragraph with deep-thought-needle inside.\n\nThird line.\n\nFourth line of reasoning.";
    assert!(crate::ui::is_long_thought(long_thought));
    view.conversation.insert_item(Item {
        item_id: "thought-1".into(),
        created_seq: "5".into(),
        turn_id: Some("main-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "thought".into(),
        content: vec![Content::text(long_thought)],
        tool: None,
    });

    // 4. A long notice (folded to 1 line by default)
    let long_notice = "Line 1 of notice\nLine 2 of notice\nLine 3 with long-notice-needle\nLine 4 of notice\nLine 5 of notice";
    assert!(crate::ui::is_long_notice(long_notice));
    view.conversation.insert_item(Item {
        item_id: "notice-1".into(),
        created_seq: "6".into(),
        turn_id: Some("main-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "notice".into(),
        content: vec![Content::text(long_notice)],
        tool: None,
    });

    // 5. An answered question (collapsed by default)
    let question: Question = serde_json::from_value(json!({
        "question_id": "q-1",
        "agent_generation_id": "g",
        "kind": "permission",
        "title": "Run cargo test?",
        "description": "Checks answered-question-needle before continuing.",
        "options": [{"option_id": "allow", "title": "Yes", "kind": "allow_once"}],
        "state": "answered",
        "answer": {"type": "permission", "option_id": "allow"},
        "fields": []
    }))
    .unwrap();
    view.conversation.questions.insert("q-1".into(), question);
    view.rebuild();

    let mut live = Live::new((720.0, 860.0));
    live.apply(&mut app, Message::Find(FindEvent::Open));

    // 1. Search folded turn
    live.apply(&mut app, Message::Find(FindEvent::Input("folded-turn-needle".into())));
    assert!(demo_view(&app).open_groups.contains("fold fold-u"), "folded turn unfolded");
    let mut current = Vec::new();
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "folded-turn-needle");

    // 2. Search grouped tool call line (.find_only() leaf)
    live.apply(&mut app, Message::Find(FindEvent::Input("grouped-call-needle".into())));
    assert!(demo_view(&app).open_groups.contains("tool-1"), "tool group unfolded");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "grouped-call-needle");

    // 3. Search closed tool call output and diff
    live.apply(&mut app, Message::Find(FindEvent::Input("closed-output-needle".into())));
    assert!(demo_view(&app).open_tools.contains("tool-2"), "tool output unfolded");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "closed-output-needle");

    live.apply(&mut app, Message::Find(FindEvent::Input("diff-line-needle".into())));
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "diff-line-needle");

    // 4. Search long thought
    live.apply(&mut app, Message::Find(FindEvent::Input("deep-thought-needle".into())));
    assert!(demo_view(&app).open_tools.contains("thought-1"), "long thought unfolded");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "deep-thought-needle");

    // 5. Search long notice
    live.apply(&mut app, Message::Find(FindEvent::Input("long-notice-needle".into())));
    assert!(demo_view(&app).open_tools.contains("notice-1"), "long notice unfolded");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "long-notice-needle");

    // 6. Search answered question details
    live.apply(&mut app, Message::Find(FindEvent::Input("answered-question-needle".into())));
    assert!(demo_view(&app).open_answers.contains("q-1"), "answered question unfolded");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "answered-question-needle");

    // 7. A query with no initial matches that later arrives via streaming inside a collapsed tool output
    // activates the newly arrived match: unfolding the tool output and disabling follow_tail.
    app.views.get_mut("demo").unwrap().follow_tail = true;
    live.apply(&mut app, Message::Find(FindEvent::Input("streamed-tool-needle".into())));
    assert!(app.find.matches.is_empty());
    assert_eq!(app.find.current, None);
    assert!(demo_view(&app).follow_tail);

    let next_seq = demo_view(&app).conversation.seq + 1;
    let epoch = demo_view(&app).lane_epoch;
    let streamed_tool = Item {
        item_id: "tool-streamed".into(),
        created_seq: next_seq.to_string(),
        turn_id: Some("main-turn".into()),
        submission_id: None,
        agent_generation_id: "g".into(),
        kind: "tool".into(),
        content: vec![],
        tool: Some(Tool {
            title: "rg check crates".into(),
            command: Some("rg check crates".into()),
            kind: "execute".into(),
            status: "completed".into(),
            detail: "arrived streamed-tool-needle in output".into(),
            diff: String::new(),
            parent_item_id: None,
        }),
    };
    live.apply(
        &mut app,
        Message::Lane(
            "demo".into(),
            epoch,
            crate::lane::Update::Event(Box::new(SessionEvent {
                session_id: "demo".into(),
                subscription_id: "sub".into(),
                seq: next_seq.to_string(),
                event: Event::ItemCreated { item: streamed_tool },
            })),
        ),
    );
    assert_eq!(app.find.matches.len(), 1);
    assert_eq!(app.find.current, Some(0));
    assert!(demo_view(&app).open_tools.contains("tool-streamed"), "streamed first match unfolds its collapsed tool output");
    assert!(!demo_view(&app).follow_tail, "streamed first match disables follow_tail");
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "streamed-tool-needle");

    // Closing Find leaves unfolded items open.
    live.apply(&mut app, Message::Find(FindEvent::Close));
    assert!(demo_view(&app).open_groups.contains("fold fold-u"));
    assert!(demo_view(&app).open_groups.contains("tool-1"));
    assert!(demo_view(&app).open_tools.contains("tool-2"));
    assert!(demo_view(&app).open_tools.contains("thought-1"));
    assert!(demo_view(&app).open_tools.contains("notice-1"));
    assert!(demo_view(&app).open_answers.contains("q-1"));
    assert!(demo_view(&app).open_tools.contains("tool-streamed"));
}

#[test]
fn find_scrolls_to_offscreen_match_in_virtualized_long_conversation() {
    use crate::find::Event as FindEvent;
    let mut app = crate::perf::long(160);
    {
        let view = app.views.get_mut("demo").unwrap();
        let item = view.conversation.item_mut("i00002").unwrap();
        item.content = vec![Content::text("Early paragraph with ancient-needle at the top.")];
        view.render("i00002");
    }
    let mut live = Live::new((720.0, 500.0));
    // Measure rows and snap to the tail of the 160-item conversation.
    live.apply(&mut app, Message::Latest);
    let redraw = || iced::Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now()));
    for m in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, m);
    }
    let mut tail_scroll = None;
    live.frame(&app, |ui| tail_scroll = scrolled(ui, "conversation"));
    assert!(tail_scroll.unwrap_or_default() > 1000.0, "at tail: {tail_scroll:?}");

    // Search for "ancient-needle" near the top of the conversation.
    live.apply(&mut app, Message::Find(FindEvent::Open));
    live.apply(&mut app, Message::Find(FindEvent::Input("ancient-needle".into())));
    assert_eq!(app.find.matches.len(), 1);
    assert_eq!(app.find.pinned_row(), Some(app.find.matches[0].row));
    assert!(!demo_view(&app).follow_tail);

    // Frame 1 materializes the pinned row, paints find rects, and runs RevealMatch -> scroll_to.
    for m in live.frame(&app, |_| {}) {
        live.apply(&mut app, m);
    }
    let mut after_scroll = None;
    let mut current = Vec::new();
    live.frame(&app, |ui| {
        after_scroll = scrolled(ui, "conversation");
        current = current_matched(ui);
    });
    assert!(after_scroll.unwrap_or(f32::MAX) < 400.0, "scrolled to reveal early row: {after_scroll:?}");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "ancient-needle");

    // When scrolled halfway through a single long message that contains multiple matches,
    // searching picks the first match at or below the top of the viewport instead of jumping
    // backward to the first occurrence at the top of that message.
    live.apply(&mut app, Message::Find(FindEvent::Close));
    {
        let view = app.views.get_mut("demo").unwrap();
        let mut long_para = String::from("repeat-needle at the very top of this long message.\n\n");
        for i in 0..40 {
            long_para.push_str(&format!("Filler paragraph {i} to make this single row tall.\n\n"));
        }
        long_para.push_str("repeat-needle near the bottom of this long message.\n");
        let item = view.conversation.item_mut("i00002").unwrap();
        item.content = vec![Content::text(&long_para)];
        view.render("i00002");
    }
    // Lay out the tall row and scroll 500px into it so the first occurrence is scrolled off the top
    // while the row itself is still top_row and the second occurrence is below top_y.
    for m in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, m);
    }
    {
        let view = app.views.get_mut("demo").unwrap();
        view.follow_tail = false;
        view.scroll_offset = 500.0;
    }
    live.frame(&app, |_| {});
    live.apply(&mut app, Message::Find(FindEvent::Open));
    live.apply(&mut app, Message::Find(FindEvent::Input("repeat-needle".into())));
    assert_eq!(app.find.matches.len(), 2);
    assert_eq!(app.find.current, Some(1), "selected the second occurrence at or below top of viewport instead of jumping back to occurrence 0");
}

#[test]
fn find_escape_terminal_focus_and_navigation_close() {
    use crate::files::Action;
    use crate::find::Event as FindEvent;
    use silli_proto::sessions::HostPath;

    let mut app = streamed();
    add_session(&mut app, "second");

    // Open Find and enter a query.
    let _ = app.update(Message::Find(FindEvent::Open));
    let _ = app.update(Message::Find(FindEvent::Input("arrow".into())));
    assert!(app.find.shown);

    // First Escape closes Find without leaving the conversation for the backend page.
    let _ = app.update(Message::Escape);
    assert!(!app.find.shown);
    assert!(!app.backend_shown());
    assert_eq!(app.find.query, "arrow", "query is remembered for next open");

    // Second Escape with Find closed goes to the backend page.
    let _ = app.update(Message::Escape);
    assert!(app.backend_shown());
    let _ = app.update(Message::Escape);
    assert!(!app.backend_shown());

    // Switching tabs closes Find.
    let _ = app.update(Message::Find(FindEvent::Open));
    assert!(app.find.shown);
    let _ = app.update(Message::Choose("second".into()));
    assert!(!app.find.shown);
    let _ = app.update(Message::Choose("demo".into()));

    // Opening a file in the reader closes Find.
    let _ = app.update(Message::Find(FindEvent::Open));
    assert!(app.find.shown);
    let _ = app.update(Message::Files(Action::Open { path: HostPath::from_bytes("/home/demo/project/README.md".as_bytes()), repo: None }));
    assert!(!app.find.shown);
    let _ = app.update(Message::Files(Action::CloseViewer));

    // When the session terminal is open and focused, Ctrl+F passes through to the terminal.
    let _events = app.update(Message::ToggleTerminal);
    {
        let pane = app.views.get_mut("demo").unwrap().terminal.as_mut().unwrap();
        pane.accept(crate::terminal::Event::Opened);
    }
    assert!(app.terminal_focused());
    let mut ui = simulator(&app, (720.0, 860.0));
    ui.send(ctrl_f());
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(!messages.iter().any(|m| matches!(m, Message::Find(FindEvent::Open))), "Ctrl+F must not open Find while terminal is focused: {messages:?}");
}

#[test]
fn find_searches_the_reader_preview_source_and_diff() {
    use crate::files::Action;
    use crate::find::{Event as FindEvent, ReaderMode, Surface};
    use silli_proto::sessions::HostPath;
    let path = |p: &str| HostPath::from_bytes(p.as_bytes());
    let mode = |app: &App| match &app.find.surface {
        Some(Surface::Reader { mode, .. }) => Some(*mode),
        _ => None,
    };
    let (mut app, _) = App::new(true, None);
    let mut live = Live::new((720.0, 860.0));
    live.apply(&mut app, Message::Files(Action::Open { path: path("/home/demo/project/README.md"), repo: None }));
    live.frame(&app, |_| {});

    // the preview: a word selected in it is the query
    let events = live.frame(&app, |ui| {
        let from = aim_at(ui, "Staying oriented", "Staying");
        let to = aim_at(ui, "Staying oriented", " oriented");
        drag(ui, from, to);
        ui.send(ctrl_f());
    });
    for m in events {
        live.apply(&mut app, m);
    }
    assert_eq!(mode(&app), Some(ReaderMode::Preview));
    assert_eq!(app.find.query, "Staying");
    assert_eq!(app.find.matches.len(), 1);
    // rendered text, so the heading's words are found without its `#`
    live.apply(&mut app, Message::Find(FindEvent::Input("session".into())));
    assert_eq!(app.find.matches.len(), 2);
    assert_eq!(app.find.current, Some(0));
    for m in live.frame(&app, |_| {}) {
        live.apply(&mut app, m);
    }
    let (mut matched, mut current) = (Vec::new(), Vec::new());
    live.frame(&app, |ui| {
        matched = find_matched(ui);
        current = current_matched(ui);
    });
    assert_eq!(matched, ["session", "session"]);
    assert_eq!(current.len(), 1);
    live.apply(&mut app, Message::Find(FindEvent::Next));
    assert_eq!(app.find.current, Some(1));

    // the same file as source: the bar stays, the matches are the source's, the current one is
    // the editor's selection, searched from the cursor
    live.apply(&mut app, Message::Files(Action::Source));
    assert!(app.find.shown);
    assert_eq!(mode(&app), Some(ReaderMode::Source));
    assert_eq!(app.find.matches.len(), 2);
    assert_eq!(app.find.current, Some(0));
    let selection = |app: &App| {
        let reader = app.views["demo"].reader.as_ref().unwrap();
        (reader.content.selection(), reader.content.cursor().position.line)
    };
    assert_eq!(selection(&app), (Some("session".into()), 0));
    live.apply(&mut app, Message::Find(FindEvent::Next));
    assert_eq!(selection(&app), (Some("session".into()), 7));
    // drawn with the editor's marks (silli patch)
    live.frame(&app, |_| {});
    assert_eq!(app.find.source_marks().iter().filter(|m| m.2).count(), 1);

    // the file read again: a new editor, and the current match is its selection again
    {
        let view = app.views.get_mut("demo").unwrap();
        view.files.reader.as_mut().unwrap().revision += 1;
        view.sync_reader();
    }
    assert_eq!(selection(&app), (None, 0), "a fresh editor");
    live.apply(&mut app, Message::Noop);
    assert_eq!(app.find.current, Some(1));
    assert_eq!(selection(&app), (Some("session".into()), 7));

    // an edit before the match: it is looked for again and stays the current one
    live.apply(&mut app, Message::Files(Action::Text(iced::widget::text_editor::Action::Move(iced::widget::text_editor::Motion::DocumentStart))));
    live.apply(&mut app, Message::Files(Action::Text(iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Insert('X')))));
    assert_eq!(app.find.matches.len(), 2);

    // Escape: the bar closes, the cursor is left where the match was
    live.apply(&mut app, Message::Escape);
    assert!(!app.find.shown);
    assert!(app.views["demo"].files.reader.is_some(), "Escape closed only the bar");
    live.apply(&mut app, Message::Files(Action::CloseViewer));
    live.apply(&mut app, Message::Files(Action::DiscardEdits));

    // the diff rows; another file closes the bar
    live.apply(&mut app, Message::Files(Action::Open { path: path("/home/demo/project/README.md"), repo: None }));
    live.apply(&mut app, Message::Find(FindEvent::Open));
    assert!(app.find.shown);
    live.apply(&mut app, Message::Files(Action::Open { path: path("/home/demo/project/src/main.rs"), repo: Some(path("/home/demo/project")) }));
    assert!(!app.find.shown);
    live.frame(&app, |_| {});
    live.apply(&mut app, Message::Find(FindEvent::Open));
    assert_eq!(mode(&app), Some(ReaderMode::Diff));
    live.apply(&mut app, Message::Find(FindEvent::Input("agents".into())));
    assert_eq!(app.find.matches.len(), 1);
    for m in live.frame(&app, |_| {}) {
        live.apply(&mut app, m);
    }
    let mut current = Vec::new();
    live.frame(&app, |ui| current = current_matched(ui));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "agents");

    // a match far along a long diff line: the diff scrolls sideways to it
    live.apply(&mut app, Message::Find(FindEvent::Close));
    {
        let view = app.views.get_mut("demo").unwrap();
        let file = view.files.reader.as_mut().unwrap();
        file.text = format!("diff --git a/x b/x\n@@ -1,1 +1,1 @@\n+{} far-needle\n", "word ".repeat(80));
        file.revision += 1;
        view.sync_reader();
    }
    live.frame(&app, |_| {});
    live.apply(&mut app, Message::Find(FindEvent::Open));
    live.apply(&mut app, Message::Find(FindEvent::Input("far-needle".into())));
    assert_eq!(app.find.matches.len(), 1);
    for m in live.frame(&app, |_| {}) {
        live.apply(&mut app, m);
    }
    let mut sideways = None;
    live.frame(&app, |ui| {
        sideways = ui.select(|candidate| match candidate {
            iced_test::selector::Candidate::Scrollable { id: Some(found), translation, .. } if *found == iced::widget::Id::new("diff-reader") => {
                Some(translation.x)
            }
            _ => None,
        });
    });
    assert!(sideways.unwrap_or_default() > 200.0, "scrolled sideways: {sideways:?}");
}

#[test]
fn find_searches_the_settings_page_and_focuses_the_agents_search() {
    use crate::find::{Event as FindEvent, Surface};
    use crate::settings_ui::{Category, Event};
    let (mut app, _) = App::new(true, None);
    let task = app.show_page(crate::Page::Settings);
    settle_task(&mut app, task);
    let mut live = Live::new((900.0, 860.0));
    let redraw = || iced::Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now()));
    live.frame(&app, |_| {});

    // Ctrl+F on the page opens the bar over it; the matches are those the page laid out
    let events = live.frame(&app, |ui| ui.send(ctrl_f()));
    for m in events {
        live.apply(&mut app, m);
    }
    assert!(app.find.shown);
    assert_eq!(app.find.surface, Some(Surface::Settings));
    live.apply(&mut app, Message::Find(FindEvent::Input("interface scale".into())));
    assert_eq!(app.find.matches.len(), 1);
    for m in live.frame(&app, |ui| ui.send(vec![redraw()])) {
        live.apply(&mut app, m);
    }
    let (mut matched, mut current) = (Vec::new(), Vec::new());
    live.frame(&app, |ui| {
        matched = find_matched(ui);
        current = current_matched(ui);
    });
    assert_eq!(matched, ["Interface scale"]);
    assert_eq!(current.len(), 1);

    // another category: the bar stays, and counts again once the page is laid out
    live.apply(&mut app, Message::Find(FindEvent::Input("session".into())));
    live.apply(&mut app, Message::Settings(Event::Category(Category::Sessions)));
    assert!(app.find.shown);
    for _ in 0..2 {
        for m in live.frame(&app, |ui| ui.send(vec![redraw()])) {
            live.apply(&mut app, m);
        }
    }
    let mut matched = Vec::new();
    live.frame(&app, |ui| matched = find_matched(ui));
    assert!(!matched.is_empty());
    assert_eq!(app.find.matches.len(), matched.len(), "the count is the page's");

    // Escape closes the bar first; the page stays
    live.apply(&mut app, Message::Escape);
    assert!(!app.find.shown);
    assert!(app.settings_page.is_some());
    live.frame(&app, |ui| matched = find_matched(ui));
    assert!(matched.is_empty(), "no highlights left: {matched:?}");

    // with no session selected: Settings is still searched
    app.selected = None;
    live.frame(&app, |_| {});
    live.apply(&mut app, Message::Find(FindEvent::Open));
    assert!(app.find.shown);
    live.apply(&mut app, Message::Find(FindEvent::Input("session".into())));
    assert!(!app.find.matches.is_empty(), "matched with no session");
    live.apply(&mut app, Message::Escape);
    app.selected = Some("demo".into());

    // Agents has its own search field: Ctrl+F puts the keys there, no bar
    let _ = app.show_page(crate::Page::Agents);
    live.frame(&app, |_| {});
    live.apply(&mut app, Message::Find(FindEvent::Open));
    assert!(!app.find.shown);
    live.frame(&app, |_| {});
    let typed = live.frame(&app, |ui| {
        use iced::keyboard::{Event, Key, Location, Modifiers, key};
        ui.send(vec![iced::Event::Keyboard(Event::KeyPressed {
            key: Key::Character("x".into()),
            modified_key: Key::Character("x".into()),
            physical_key: key::Physical::Code(key::Code::KeyX),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: Some("x".into()),
            repeat: false,
        })]);
    });
    assert!(typed.iter().any(|m| format!("{m:?}").contains("Search(\"x\")")), "{typed:?}");
}
