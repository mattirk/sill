use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};
use sill_proto::{Client, sfp::*};

use crate::{
    App, Message, SessionChoice,
    backend::Update,
    demo,
    model::{Conversation, item_text},
};

fn event(view: &Conversation, payload: Event) -> SessionEvent {
    SessionEvent {
        session_id: view.session_id.clone(),
        subscription_id: view.subscription_id.clone(),
        seq: (view.seq + 1).to_string(),
        event: payload,
    }
}

#[test]
fn duplicate_foreign_and_missing_events_do_not_corrupt_history() {
    let mut view = demo::conversation();
    let append = demo::append(4, "hello 🦀");
    assert!(view.accept(append.clone()).unwrap());
    assert!(!view.accept(append).unwrap());
    let before = view.transcript();
    let mut foreign = demo::append(5, "wrong");
    foreign.subscription_id = "old-subscription".into();
    assert!(!view.accept(foreign).unwrap());
    assert!(view.accept(demo::append(6, "gap")).is_err());
    assert_eq!(view.seq, 4);
    assert_eq!(view.transcript(), before);
    assert!(view.accept(demo::append(5, " contiguous")).unwrap());
}

#[test]
fn older_item_appends_stay_in_place_and_unknown_blocks_have_text_fallback() {
    let mut view = demo::conversation();
    let mut later = view.items["agent"].clone();
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
    assert!(item_text(&view.items["agent"]).ends_with("arrived later"));
    assert_eq!(item_text(&view.items["later"]), "fallback");
}

#[test]
fn queued_prompt_becomes_visible_on_dispatch_and_question_resolves() {
    let mut view = demo::conversation();
    view.items.get_mut("user").unwrap().submission_id = Some("queued".into());
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
    assert!(view.questions.values().any(|q| q.state == "open"));
    let mut question = view.questions["demo-question"].clone();
    question.state = "answered".into();
    view.accept(event(&view, Event::QuestionUpdated { question }))
        .unwrap();
    assert!(!view.questions.values().any(|q| q.state == "open"));
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
                    session: source.session.unwrap(),
                }],
                next_page: Some("first-fragment".into()),
                fragment: None,
                expires_at: None,
            },
        );
        let record = serde_json::to_string(&Record::Item {
            item: source.items["agent"].clone(),
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
    let mut view = Conversation::attach(&client, "demo").unwrap();
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
                .unwrap(),
            expected
        );
    }
    assert!(item_text(&view.items["agent"]).ends_with(" live 🦀"));
    assert!(!view.transcript().contains("duplicate"));
    server.join().unwrap();
}

#[test]
fn session_switch_ignores_old_snapshots_and_errors() {
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Choose(SessionChoice {
        id: "new".into(),
        title: "New".into(),
        agent: "codex".into(),
    }));
    let _ = app.update(Message::Backend(
        0,
        Update::Snapshot(Box::new(demo::conversation())),
    ));
    let _ = app.update(Message::Backend(0, Update::Error("old error".into())));
    assert_eq!(app.selected.as_ref().unwrap().id, "new");
    assert!(app.conversation.items.is_empty());
    assert!(app.error.is_none());
}

#[test]
fn transcript_selection_is_read_only_and_stable_during_streaming() {
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::ToggleText);
    let original = app.text_view.as_ref().unwrap().text();
    let _ = app.update(Message::TextAction(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Insert('X')),
    ));
    let _ = app.update(Message::DemoTick);
    assert_eq!(app.text_view.as_ref().unwrap().text(), original);
    let _ = app.update(Message::RefreshText);
    assert!(
        app.text_view
            .as_ref()
            .unwrap()
            .text()
            .contains("A streamed paragraph")
    );
    assert!(!app.text_view.as_ref().unwrap().text().starts_with('X'));
}

/// The app as a live client would be, its requests captured instead of sent.
fn live_app() -> (App, std::sync::mpsc::Receiver<crate::backend::Command>) {
    let (mut app, _) = App::new(true, None);
    app.demo = false;
    let (tx, rx) = std::sync::mpsc::channel();
    app.commands = Some(tx);
    (app, rx)
}

fn sent(rx: &std::sync::mpsc::Receiver<crate::backend::Command>) -> (&'static str, Value) {
    match rx.try_recv().expect("a request") {
        crate::backend::Command::Call { method, params, .. } => (method, params),
        other => panic!("expected a call, got {other:?}"),
    }
}

/// Each action is one public SFP request of the exact shape the daemon accepts (its request
/// structs refuse unknown fields): send, stop, answer, a mode, an option, unqueue.
#[test]
fn actions_send_the_protocol_requests() {
    use iced::widget::text_editor::{Action, Edit};
    let (mut app, rx) = live_app();
    // an empty composer sends nothing
    let _ = app.update(Message::Send);
    assert!(rx.try_recv().is_err());
    let _ = app.update(Message::Prompt(Action::Edit(Edit::Paste(
        std::sync::Arc::new("hello agent\nsecond line".into()),
    ))));
    let _ = app.update(Message::Send);
    let (method, params) = sent(&rx);
    assert_eq!(method, "_sill/sessions.submit");
    assert_eq!(params["session_id"], "demo");
    assert_eq!(params["text"], "hello agent\nsecond line");
    assert_eq!(params["attachments"], json!([]));
    assert!(
        params["submission_id"]
            .as_str()
            .unwrap()
            .starts_with("gui-prompt-")
    );
    serde_json::from_value::<Submit>(params).unwrap();
    assert!(
        app.prompt.text().trim().is_empty(),
        "the composer clears on send"
    );

    let _ = app.update(Message::Stop);
    let (method, params) = sent(&rx);
    assert_eq!(method, "_sill/sessions.cancel");
    assert_eq!(params, json!({"session_id":"demo", "turn_id":"demo-turn"}));

    let _ = app.update(Message::Answer {
        question_id: "q1".into(),
        option_id: "allow".into(),
    });
    let (method, params) = sent(&rx);
    assert_eq!(method, "_sill/questions.answer");
    assert_eq!(
        params["answer"],
        json!({"type":"permission", "option_id":"allow"})
    );
    assert!(
        params["answer_id"]
            .as_str()
            .unwrap()
            .starts_with("gui-answer-")
    );
    serde_json::from_value::<AnswerQuestion>(params).unwrap();

    let pick = |value: &str, boolean| crate::Pick {
        value: value.into(),
        title: value.into(),
        boolean,
    };
    let _ = app.update(Message::SetMode(pick("plan", None)));
    let (method, params) = sent(&rx);
    assert_eq!(method, "_sill/sessions.control");
    assert_eq!(
        params["control"],
        json!({"type":"set_mode", "mode_id":"plan"})
    );
    let _ = app.update(Message::SetOption("web".into(), pick("true", Some(true))));
    let (_, params) = sent(&rx);
    assert_eq!(
        params["control"],
        json!({"type":"set_config", "option_id":"web", "value":{"type":"bool", "value":true}})
    );
    let _ = app.update(Message::SetOption("model".into(), pick("brisk", None)));
    let (_, params) = sent(&rx);
    assert_eq!(
        params["control"]["value"],
        json!({"type":"id", "value":"brisk"})
    );

    let _ = app.update(Message::Unqueue("s1".into()));
    let (method, params) = sent(&rx);
    assert_eq!(method, "_sill/submissions.cancel");
    assert_eq!(params, json!({"session_id":"demo", "submission_id":"s1"}));
}

/// A refused request comes back as a failure the person sees; the connection stays and the
/// next request goes out on it.
#[test]
fn a_refused_request_is_reported_and_the_connection_stays() {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        let list = read_request(&mut reader, "_sill/sessions.list");
        reply(&mut peer, &list, json!({"sessions": [], "next": null}));
        let submit = read_request(&mut reader, "_sill/sessions.submit");
        assert_eq!(submit["params"]["text"], "hi");
        writeln!(
            peer,
            "{}",
            json!({"jsonrpc":"2.0", "id":submit["id"],
                   "error":{"code":-32000, "message":"the session has ended"}})
        )
        .unwrap();
        let cancel = read_request(&mut reader, "_sill/sessions.cancel");
        reply(&mut peer, &cancel, json!({"requested": true}));
        let again = read_request(&mut reader, "_sill/sessions.list");
        reply(&mut peer, &again, json!({"sessions": [], "next": null}));
    });
    let socket = std::sync::Mutex::new(Some(socket));
    let (commands, updates) = crate::backend::start_with(None, move || {
        let stream = socket.lock().unwrap().take().expect("one connection");
        Client::over(stream)
    });
    let next = |want: &dyn Fn(&Update) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match updates.try_recv() {
                Ok((_, update)) if want(&update) => return update,
                Ok(_) => {}
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => panic!("no such update in time"),
            }
        }
    };
    next(&|u| matches!(u, Update::Status(s) if s == "Connected"));
    commands
        .send(crate::backend::Command::Call {
            method: "_sill/sessions.submit",
            params: json!({"session_id":"x", "submission_id":"y", "text":"hi", "attachments":[]}),
            what: "Sending the prompt",
        })
        .unwrap();
    let Update::Failed(failure) = next(&|u| matches!(u, Update::Failed(_))) else {
        unreachable!()
    };
    assert!(
        failure.starts_with("Sending the prompt:") && failure.contains("the session has ended"),
        "{failure}"
    );
    commands
        .send(crate::backend::Command::Call {
            method: "_sill/sessions.cancel",
            params: json!({"session_id":"x", "turn_id":"t"}),
            what: "Stopping the turn",
        })
        .unwrap();
    commands.send(crate::backend::Command::Refresh).unwrap();
    next(&|u| matches!(u, Update::Sessions(_)));
    server.join().unwrap();
}

/// The bundled variable faces are found at ordinary and bold weights (before 2026-11-04 every
/// "Source Code Pro" span fell back to another font: fontdb listed it only at 200).
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
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 860.0), app.view());
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
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 860.0), app.view());
    ui.point_at(iced::Point::new(450.0, 18.0));
    let _ = ui.simulate(iced_test::simulator::click());
    ui.click("witty-willow").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(messages.first(), Some(Message::DragWindow)),
        "{messages:?}"
    );
    assert!(
        matches!(messages.get(1), Some(Message::Choose(choice)) if choice.title == "witty-willow"),
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
        crate::quit_key(press(Modifiers::CTRL)),
        Some(Message::Quit)
    ));
    assert!(crate::quit_key(press(Modifiers::empty())).is_none());
}

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

/// The terminal is the daemon's byte stream through the emulator: output reaches the screen with
/// its colours, keys and the emulator's own answers go back, a resize is a request, and the end
/// of the stream is the end of the shell.
#[test]
fn a_terminal_shows_its_stream_and_sends_keys_back() {
    use crate::terminal::{Connection, Control, Event, Pane, Status};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    let (ours, mut remote) = UnixStream::pair().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let controls = Arc::new(Mutex::new(Vec::new()));
    let seen = controls.clone();
    let close = ours.try_clone().unwrap();
    let (mut pane, events) = Pane::open(40, 10, move |cols, rows| {
        assert_eq!((cols, rows), (40, 10));
        Ok(Connection {
            discarded: true,
            size: (cols, rows),
            stream: sill_proto::transport::RawStream {
                reader: Box::new(ours.try_clone().unwrap()),
                writer: Box::new(ours),
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
        })
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
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 860.0), app.view());
    ui.click(">_").unwrap();
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        matches!(messages.as_slice(), [Message::ToggleTerminal]),
        "{messages:?}"
    );
    // the task carries the pane's events; dropping it would hang the shell up
    let _events = app.update(Message::ToggleTerminal);
    let pane = app.terminal.as_mut().unwrap();
    pane.accept(crate::terminal::Event::Opened);
    settle(pane, "project$");
    let mut ui =
        iced_test::Simulator::with_size(crate::snapshots::settings(), (720.0, 860.0), app.view());
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
        !app.terminal_shown && app.terminal.is_some(),
        "hiding only detaches the view"
    );
}
