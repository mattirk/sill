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
