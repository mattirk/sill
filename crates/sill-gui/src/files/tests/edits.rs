//! The reader's source: editing, the unsaved-edits guard, save, the extent and history.
use super::*;

#[test]
fn source_is_editable_and_binary_file_errors_are_visible() {
    let mut app = app();
    let file = path("/home/demo/project/README.md");
    act(
        &mut app,
        Action::Open {
            path: file.clone(),
            repo: None,
        },
    );
    act(&mut app, Action::Source);
    let original = app.views["demo"].reader.as_ref().unwrap().content.text();
    assert!(original.contains("Your session's files"));
    act(
        &mut app,
        Action::Text(text_editor::Action::Edit(text_editor::Edit::Insert('X'))),
    );
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        format!("X{original}")
    );
    assert!(app.views["demo"].reader.as_ref().unwrap().dirty);
    let open = app.views.get_mut("demo").unwrap().files.open(file, None);
    let (serial, open) = pending(&mut app, "demo", open);
    reply(&mut app, "demo", serial, open, Err("Binary file".into()));
    assert_eq!(
        files(&app, "demo")
            .reader
            .as_ref()
            .unwrap()
            .error
            .as_deref(),
        Some("Binary file")
    );
}

#[test]
fn unsaved_source_survives_navigation_and_close_requires_explicit_discard() {
    let mut app = app();
    open_source(&mut app);
    insert(&mut app, 'X');
    let edited = app.views["demo"].reader.as_ref().unwrap().content.text();
    let _ = app.update(Message::Choose("b".into()));
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
    let _ = app.update(Message::Escape);
    assert!(app.views["demo"].reader.as_ref().unwrap().dirty);
    assert!(app.drawer.pending_discard.is_some());
    act(&mut app, Action::CancelDiscard);
    assert!(app.drawer.pending_discard.is_none());
    act(&mut app, Action::RefreshFile);
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
    act(&mut app, Action::CancelDiscard);
    let _ = app.update(Message::CloseRequested);
    assert!(app.drawer.pending_discard.is_some());
    act(&mut app, Action::CancelDiscard);
    act(
        &mut app,
        Action::Open {
            path: path("/home/demo/project/README.md"),
            repo: None,
        },
    );
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
    act(&mut app, Action::DiscardEdits);
    assert!(!app.views["demo"].reader.as_ref().unwrap().dirty);
    assert!(
        app.views["demo"]
            .files
            .reader
            .as_ref()
            .unwrap()
            .path
            .display
            .ends_with("README.md")
    );
}
#[test]
fn markdown_preview_and_diff_are_read_only_but_source_edits_survive_preview() {
    let mut app = app();
    act(
        &mut app,
        Action::Open {
            path: path("/home/demo/project/README.md"),
            repo: None,
        },
    );
    let original = app.views["demo"].reader.as_ref().unwrap().content.text();
    insert(&mut app, 'X');
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        original
    );
    act(&mut app, Action::Source);
    insert(&mut app, 'X');
    act(&mut app, Action::Source);
    assert!(
        app.views["demo"]
            .reader
            .as_ref()
            .unwrap()
            .markdown
            .is_some()
    );
    assert!(app.views["demo"].reader.as_ref().unwrap().dirty);
    insert(&mut app, 'Y');
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        format!("X{original}")
    );
    act(&mut app, Action::CloseViewer);
    act(&mut app, Action::DiscardEdits);
    act(
        &mut app,
        Action::Open {
            path: path("/home/demo/project/src/main.rs"),
            repo: Some(path("/home/demo/project")),
        },
    );
    act(&mut app, Action::Source);
    let original = app.views["demo"].reader.as_ref().unwrap().content.text();
    insert(&mut app, 'Z');
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        original
    );
    assert!(!app.views["demo"].reader.as_ref().unwrap().dirty);
}
#[test]
fn save_writes_exact_source_and_failed_save_keeps_the_edits() {
    use crate::tests::{connector, daemon, settle_task};
    let (client, mut peer) = daemon();
    let mut app = app();
    open_source(&mut app);
    // Exercise CRLF, Unicode and no final newline through the editor and wire API.
    {
        let view = app.views.get_mut("demo").unwrap();
        let r = view.reader.as_mut().unwrap();
        r.saved = "let café = 1;\r\n// end".into();
        r.content = crate::editor::Editor::with_text(&r.saved);
        view.files.reader.as_mut().unwrap().text = r.saved.clone();
    }
    insert(&mut app, 'X');
    let expected = app.views["demo"].reader.as_ref().unwrap().content.text();
    assert_eq!(expected, "Xlet café = 1;\r\n// end");
    let wire = expected.clone();
    app.actions = Some(sill_client::actions::Actions::new(connector(vec![client])));
    let server = std::thread::spawn(move || {
        let request = peer.expect("_sill/files.write");
        assert_eq!(
            request["params"]["path"]["display"],
            "/home/demo/project/src/main.rs"
        );
        let bytes =
            sill_proto::admin::decode_bytes(request["params"]["bytes_base64"].as_str().unwrap())
                .unwrap();
        assert_eq!(bytes, wire.as_bytes());
        peer.reply(&request, serde_json::json!({}));
        let request = peer.expect("_sill/files.write");
        peer.refuse(&request, "permission_denied", "read-only file");
    });
    let task = app.update(Message::Files(Action::SaveFile));
    settle_task(&mut app, task);
    assert!(!app.views["demo"].reader.as_ref().unwrap().dirty);
    assert_eq!(
        app.views["demo"].files.reader.as_ref().unwrap().text,
        expected
    );
    act(&mut app, Action::History(false));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        "let café = 1;\r\n// end"
    );
    assert!(app.views["demo"].reader.as_ref().unwrap().dirty);
    act(&mut app, Action::History(true));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        expected
    );
    assert!(!app.views["demo"].reader.as_ref().unwrap().dirty);
    insert(&mut app, 'Y');
    let second = app.views["demo"].reader.as_ref().unwrap().content.text();
    let task = app.update(Message::Files(Action::SaveFile));
    settle_task(&mut app, task);
    let r = app.views["demo"].reader.as_ref().unwrap();
    assert!(r.dirty);
    assert_eq!(r.content.text(), second);
    assert!(r.notice.as_deref().unwrap().contains("read-only file"));
    server.join().unwrap();
}
#[test]
fn a_save_reply_does_not_mark_newer_edits_saved_or_overwrite_another_file() {
    use sill_client::actions::Outcome;
    let mut app = app();
    open_source(&mut app);
    insert(&mut app, 'X');
    let r = app.views.get_mut("demo").unwrap().reader.as_mut().unwrap();
    let saved = r.content.text();
    let key = r.key.clone();
    r.saving = Some(900);
    insert(&mut app, 'Y');
    let edited = app.views["demo"].reader.as_ref().unwrap().content.text();
    act(
        &mut app,
        Action::Saved {
            session: "demo".into(),
            key: key.clone(),
            serial: 900,
            text: saved.clone(),
            outcome: Outcome::Done(serde_json::json!({})),
        },
    );
    let r = app.views["demo"].reader.as_ref().unwrap();
    assert!(r.dirty);
    assert_eq!(r.saved, saved);
    assert_eq!(r.content.text(), edited);
    act(
        &mut app,
        Action::Saved {
            session: "demo".into(),
            key,
            serial: 900,
            text: "stale".into(),
            outcome: Outcome::Done(serde_json::json!({})),
        },
    );
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
}

#[test]
fn an_ended_session_keeps_its_unsaved_buffer_and_uncertain_save_does_not_clear_it() {
    use sill_client::actions::Outcome;
    let mut app = app();
    open_source(&mut app);
    insert(&mut app, 'X');
    let r = app.views.get_mut("demo").unwrap().reader.as_mut().unwrap();
    let edited = r.content.text();
    let key = r.key.clone();
    r.saving = Some(800);
    act(
        &mut app,
        Action::Saved {
            session: "demo".into(),
            key,
            serial: 800,
            text: edited.clone(),
            outcome: Outcome::Uncertain("connection lost".into()),
        },
    );
    let r = app.views["demo"].reader.as_ref().unwrap();
    assert!(r.dirty);
    assert_eq!(r.content.text(), edited);
    assert!(r.notice.as_deref().unwrap().contains("outcome unknown"));
    let _ = app.listed(Ok(vec![]));
    assert!(app.tabs.iter().any(|t| t.id == "demo"));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
}

#[test]
fn source_extent_accounts_for_tabs_wide_characters_and_edits() {
    assert_eq!(super::source_columns("a\tb"), 9);
    assert_eq!(super::source_columns("12345678\tx"), 17);
    assert_eq!(super::source_columns("a\n你好\ne\u{301}"), 4);
    let mut app = app();
    open_source(&mut app);
    act(&mut app, Action::Text(text_editor::Action::SelectAll));
    act(
        &mut app,
        Action::Text(text_editor::Action::Edit(text_editor::Edit::Paste(
            std::sync::Arc::new("x".repeat(300)),
        ))),
    );
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().source_columns,
        300
    );
    act(&mut app, Action::Text(text_editor::Action::SelectAll));
    act(
        &mut app,
        Action::Text(text_editor::Action::Edit(text_editor::Edit::Paste(
            std::sync::Arc::new("short".into()),
        ))),
    );
    assert_eq!(app.views["demo"].reader.as_ref().unwrap().source_columns, 5);
}

#[test]
fn source_history_restores_selection_extent_and_respects_preview_and_reload() {
    let mut app = app();
    open_source(&mut app);
    let original = app.views["demo"].reader.as_ref().unwrap().content.text();
    act(&mut app, Action::Text(text_editor::Action::SelectAll));
    let cursor = app.views["demo"].reader.as_ref().unwrap().content.cursor();
    act(
        &mut app,
        Action::Text(text_editor::Action::Edit(text_editor::Edit::Paste(
            std::sync::Arc::new("long".repeat(100)),
        ))),
    );
    act(&mut app, Action::History(false));
    let r = app.views["demo"].reader.as_ref().unwrap();
    assert_eq!(r.content.text(), original);
    assert_eq!(r.content.cursor(), cursor);
    assert!(!r.dirty);
    assert_eq!(r.source_columns, source_columns(&original));
    act(&mut app, Action::History(true));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().source_columns,
        400
    );
    act(&mut app, Action::Source);
    act(&mut app, Action::History(false));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        "long".repeat(100)
    );
    act(&mut app, Action::Source);
    act(&mut app, Action::History(false));
    act(&mut app, Action::RefreshFile);
    let refreshed = app.views["demo"].reader.as_ref().unwrap().content.text();
    act(&mut app, Action::History(true));
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        refreshed
    );
}
