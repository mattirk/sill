//! Files in the window: each session keeps its own folders and reader across refreshes and tab
//! switches, and a reply only changes the session it was asked for.
use super::*;
use super::links::{FindHeading, LinkTarget, heading_index, resolve_link};
use iced::widget::markdown;
use sill_client::files::{Data, Request, key};
use sill_proto::admin;

use crate::{App, Message, SessionView, Tab};

fn path(p: &str) -> HostPath {
    HostPath::from_path(std::path::Path::new(p))
}

fn metadata(directory: bool) -> admin::FileMetadata {
    admin::FileMetadata {
        file: !directory,
        directory,
        symlink: false,
        target: None,
        length: "1".into(),
        readonly: false,
    }
}

/// The demo, with a second open session "b".
fn app() -> App {
    let (mut app, _) = App::new(true, None);
    app.tabs.push(Tab {
        id: "b".into(),
        title: "b".into(),
        agent: "codex".into(),
        running: true,
    });
    app.views.insert("b".into(), SessionView::new("b"));
    app
}

fn act(app: &mut App, action: Action) {
    let _ = app.update(Message::Files(action));
}

/// A request for the demo session made by hand, so its reply can come late; its serial.
fn pending(app: &mut App, session: &str, request: Request) -> (u64, Request) {
    app.drawer.serial += 1;
    let serial = app.drawer.serial;
    let request = app
        .views
        .get_mut(session)
        .unwrap()
        .files
        .prepare(serial, request);
    (serial, request)
}

fn reply(
    app: &mut App,
    session: &str,
    serial: u64,
    request: Request,
    result: Result<Data, String>,
) {
    act(
        app,
        Action::Reply(Box::new(Reply {
            session: session.into(),
            serial,
            request,
            result,
        })),
    );
}

fn files<'a>(app: &'a App, session: &str) -> &'a sill_client::files::FilesState {
    &app.views[session].files
}

#[test]
fn refresh_and_session_switch_keep_expansion_and_the_open_file() {
    let mut app = app();
    let folder = path("/home/demo/project/src");
    let file = path("/home/demo/project/src/main.rs");
    act(&mut app, Action::Toggle(folder.clone()));
    act(
        &mut app,
        Action::Open {
            path: file.clone(),
            repo: None,
        },
    );
    act(&mut app, Action::Refresh);
    assert!(files(&app, "demo").expanded.contains(key(&folder)));
    let _ = app.update(Message::Choose("b".into()));
    assert!(!files(&app, "b").expanded.contains(key(&folder)));
    assert!(files(&app, "b").reader.is_none());
    let _ = app.update(Message::Choose("demo".into()));
    assert!(files(&app, "demo").expanded.contains(key(&folder)));
    assert_eq!(
        key(&files(&app, "demo").reader.as_ref().unwrap().path),
        key(&file)
    );
    assert!(
        app.views["demo"].reader.is_some(),
        "its rendering came back too"
    );
    act(&mut app, Action::Toggle(folder.clone()));
    act(&mut app, Action::Refresh);
    assert!(
        !files(&app, "demo").expanded.contains(key(&folder)),
        "refresh must not reopen a collapsed folder"
    );
}

/// The 15 s refresh reloads every open folder and pin: until it answers, the drawer shows what it
/// showed, without "Loading…" rows or notes coming and going (a visible flicker, 2026-11-05).
#[test]
fn a_reload_keeps_the_tree_as_it_was_until_it_answers() {
    let mut app = app();
    let folder = path("/home/demo/project/src");
    act(&mut app, Action::Toggle(folder.clone()));
    let listed = files(&app, "demo").directories[key(&folder)].entries.len();
    assert!(listed > 0);
    let requests = app.views.get_mut("demo").unwrap().files.refresh();
    let mut awaited = vec![];
    for request in requests {
        awaited.push(pending(&mut app, "demo", request));
    }
    let state = files(&app, "demo");
    let dir = &state.directories[key(&folder)];
    assert!(
        dir.pending.is_some() && dir.reloading,
        "a reload, not a first load"
    );
    assert_eq!(dir.entries.len(), listed, "the old listing stays meanwhile");
    assert!(state.roots_pending.is_some() && state.initialized);
    for (serial, request) in awaited {
        let result = crate::demo::files(&request);
        reply(&mut app, "demo", serial, request, result);
    }
    let dir = &files(&app, "demo").directories[key(&folder)];
    assert!(dir.pending.is_none() && !dir.reloading);
    // a folder's first listing is not a reload: it says it is loading
    let fresh = path("/home/demo/project/docs");
    let first = Request::Directory {
        path: fresh.clone(),
        after: None,
        pages: 1,
    };
    let _ = pending(&mut app, "demo", first);
    assert!(!files(&app, "demo").directories[key(&fresh)].reloading);
}

#[test]
fn late_file_reply_cannot_replace_a_newer_file_or_reopen_a_closed_viewer() {
    let mut app = app();
    let a = path("/a.md");
    let b = path("/b.md");
    let open = app
        .views
        .get_mut("demo")
        .unwrap()
        .files
        .open(a.clone(), None);
    let (old, open) = pending(&mut app, "demo", open);
    let newer = app
        .views
        .get_mut("demo")
        .unwrap()
        .files
        .open(b.clone(), None);
    let (serial, newer) = pending(&mut app, "demo", newer);
    reply(
        &mut app,
        "demo",
        old,
        open,
        Ok(Data::File("old content".into())),
    );
    let reader = files(&app, "demo").reader.as_ref().unwrap();
    assert_eq!(key(&reader.path), key(&b));
    assert!(reader.text.is_empty());
    act(&mut app, Action::CloseViewer);
    reply(
        &mut app,
        "demo",
        serial,
        newer,
        Ok(Data::File("late".into())),
    );
    assert!(files(&app, "demo").reader.is_none());
}

#[test]
fn a_background_reply_only_updates_its_own_session() {
    let mut app = app();
    let file = path("/old.md");
    let open = app.views.get_mut("demo").unwrap().files.open(file, None);
    let (serial, open) = pending(&mut app, "demo", open);
    let _ = app.update(Message::Choose("b".into()));
    reply(
        &mut app,
        "demo",
        serial,
        open,
        Ok(Data::File("belongs to the demo".into())),
    );
    assert!(files(&app, "b").reader.is_none());
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().text,
        "belongs to the demo"
    );
}

/// Regression (review of 934097b): browsing a folder in one session, switching to another and
/// closing its drawer, then the first session's reply arriving, opened the drawer over the other
/// session — the reply set the window's shared flag. A reply now changes only its own session.
#[test]
fn a_background_reply_never_opens_the_drawer_over_another_session() {
    let mut app = app();
    act(&mut app, Action::ToggleDrawer);
    assert!(app.drawer.shown);
    let browsed = path("/home/demo/elsewhere");
    let (serial, request) = pending(&mut app, "demo", Request::Inspect(browsed.clone()));
    let _ = app.update(Message::Choose("b".into()));
    act(&mut app, Action::ToggleDrawer);
    assert!(!app.drawer.shown);
    reply(
        &mut app,
        "demo",
        serial,
        request,
        Ok(Data::Metadata(metadata(true))),
    );
    assert!(
        !app.drawer.shown,
        "the other session's drawer stays closed"
    );
    let demo = files(&app, "demo");
    assert!(
        demo.expanded.contains(key(&browsed)),
        "the demo session still has its answer"
    );
    assert_eq!(key(demo.home.as_ref().unwrap()), key(&browsed));
}

/// Closing the drawer cancels a folder reveal still on its way, in the session itself too.
#[test]
fn closing_the_drawer_cancels_a_reveal_on_its_way() {
    let mut app = app();
    act(&mut app, Action::ToggleDrawer);
    let (serial, request) = pending(&mut app, "demo", Request::Inspect(path("/somewhere")));
    act(&mut app, Action::ToggleDrawer);
    reply(
        &mut app,
        "demo",
        serial,
        request,
        Ok(Data::Metadata(metadata(true))),
    );
    assert!(!app.drawer.shown);
}

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
    assert!(original.contains("A workspace for your session"));
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
fn automatic_drawer_follows_repo_transitions_and_preserves_manual_choice() {
    let (mut drawer, _) = Drawer::new(None);
    let mut files = files::FilesState::default();
    let pin = path("/repo");
    files.roots.push(files::Root {
        path: pin.clone(),
        global: false,
        metadata: Some(metadata(true)),
        error: None,
    });
    files.repos.insert(
        key(&pin).into(),
        files::Repo {
            root: Some(pin.clone()),
            read: true,
            ..Default::default()
        },
    );
    files.diff_mode = true;
    drawer.follow("a", &mut files, true);
    assert!(!files.diff_mode, "a clean repository selects Files");
    files.diff_mode = true;
    drawer.follow("a", &mut files, true);
    assert!(
        files.diff_mode,
        "a manual choice holds through an unchanged scan"
    );
    files
        .repos
        .get_mut(key(&pin))
        .unwrap()
        .entries
        .push(sill_proto::discovery::StatusEntry {
            path: path("/repo/file"),
            status: " M".into(),
        });
    files.diff_mode = false;
    drawer.follow("a", &mut files, true);
    assert!(files.diff_mode, "a dirty transition selects Diff");
    files.repos.get_mut(key(&pin)).unwrap().entries.clear();
    drawer.follow("a", &mut files, true);
    assert!(!files.diff_mode, "cleaning the repository restores Files");
    files.diff_mode = true;
    drawer.follow("a", &mut files, false);
    assert!(files.diff_mode, "disabled leaves the manual view alone");
    drawer.follow("a", &mut files, true);
    assert!(!files.diff_mode, "enabling starts fresh");
}

fn insert(app: &mut App, c: char) {
    act(
        app,
        Action::Text(text_editor::Action::Edit(text_editor::Edit::Insert(c))),
    );
}
fn open_source(app: &mut App) {
    act(
        app,
        Action::Open {
            path: path("/home/demo/project/src/main.rs"),
            repo: None,
        },
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

#[test]
fn folder_picker_pins_only_the_originating_session() {
    let mut app = app();
    act(&mut app, Action::PickFolder);
    let (session, token) = app.drawer.folder_picker.clone().unwrap();
    assert_eq!(session, "demo");
    app.selected = Some("b".into());
    act(&mut app, Action::PickFolder);
    assert_eq!(
        app.drawer.folder_picker,
        Some((session.clone(), token.clone()))
    );
    act(
        &mut app,
        Action::FolderPicked {
            session,
            token,
            path: Some("/home/demo/project".into()),
        },
    );
    assert!(app.drawer.folder_picker.is_none());
    // The demo refuses pin requests: its error proves the request reached the original tab.
    assert!(
        app.views["demo"]
            .files
            .error
            .as_deref()
            .unwrap()
            .contains("Demo pins")
    );
    assert!(app.views["b"].files.error.is_none());
}

#[test]
fn folder_picker_cancel_and_removed_session_do_nothing() {
    let mut app = app();
    act(&mut app, Action::PickFolder);
    let (session, token) = app.drawer.folder_picker.clone().unwrap();
    act(
        &mut app,
        Action::FolderPicked {
            session,
            token,
            path: None,
        },
    );
    assert!(app.drawer.folder_picker.is_none());
    assert!(app.views["demo"].files.error.is_none());

    act(&mut app, Action::PickFolder);
    let (session, token) = app.drawer.folder_picker.clone().unwrap();
    app.views.remove(&session);
    act(
        &mut app,
        Action::FolderPicked {
            session,
            token,
            path: Some("/home/demo/project".into()),
        },
    );
    assert!(app.drawer.folder_picker.is_none());
    assert!(app.views["b"].files.error.is_none());
}

#[test]
fn folder_picker_ignores_a_replaced_session_view() {
    let mut app = app();
    act(&mut app, Action::PickFolder);
    let (session, token) = app.drawer.folder_picker.clone().unwrap();
    app.views
        .insert(session.clone(), SessionView::new(&session));
    act(
        &mut app,
        Action::FolderPicked {
            session: session.clone(),
            token,
            path: Some("/home/demo/project".into()),
        },
    );
    assert!(app.drawer.folder_picker.is_none());
    assert!(app.views[&session].files.error.is_none());
}

#[test]
fn document_links_resolve_against_the_document_not_the_process_directory() {
    let source = path("/project/docs/start.md");
    for (target, expected, anchor) in [
        ("other.md", "/project/docs/other.md", ""),
        (
            "../README.md#some-heading",
            "/project/README.md",
            "some-heading",
        ),
        ("./a%20b.md#caf%C3%A9", "/project/docs/a b.md", "café"),
        ("#here", "/project/docs/start.md", "here"),
        ("/elsewhere/file.md", "/elsewhere/file.md", ""),
    ] {
        let LinkTarget::File(actual, fragment) = resolve_link(&source, target).unwrap() else {
            panic!("expected file")
        };
        assert_eq!(actual.to_path().unwrap(), std::path::Path::new(expected));
        assert_eq!(fragment, anchor);
    }
    assert!(matches!(
        resolve_link(&source, "https://example.org/a#b").unwrap(),
        LinkTarget::Web(_)
    ));
    for target in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "//example.org/a",
        "%00.md",
    ] {
        assert!(resolve_link(&source, target).is_err(), "{target}");
    }
}

#[test]
fn heading_links_match_formatted_unicode_and_duplicate_headings() {
    let content =
        markdown::Content::parse("# Hello, **world**!\n\n# Hello, world!\n\n# Café `code`\n");
    assert_eq!(heading_index(content.items(), "hello-world"), Some(0));
    assert_eq!(heading_index(content.items(), "hello-world-1"), Some(1));
    assert_eq!(heading_index(content.items(), "café-code"), Some(2));
    assert_eq!(heading_index(content.items(), "missing"), None);
}

#[test]
fn document_links_keep_history_guard_edits_and_ignore_stale_sources() {
    let mut app = app();
    let first = path("/home/demo/project/README.md");
    act(
        &mut app,
        Action::Open {
            path: first.clone(),
            repo: None,
        },
    );
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: first.clone(),
            target: "docs/next.md".into(),
        },
    );
    let second = files(&app, "demo").reader.as_ref().unwrap().path.clone();
    assert_eq!(second.display, "/home/demo/project/docs/next.md");
    act(&mut app, Action::Back(false));
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        first.display
    );
    act(&mut app, Action::Back(true));
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: first.clone(),
            target: "stale.md".into(),
        },
    );
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    // Mark a preview buffer dirty, as when editing source then switching back to preview.
    app.views
        .get_mut("demo")
        .unwrap()
        .reader
        .as_mut()
        .unwrap()
        .dirty = true;
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: second.clone(),
            target: "third.md".into(),
        },
    );
    assert!(app.drawer.pending_discard.is_some());
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    act(&mut app, Action::CancelDiscard);
    act(&mut app, Action::Back(false));
    assert!(app.drawer.pending_discard.is_some());
    assert_eq!(app.drawer.navigation["demo"].back.len(), 1);
}

#[test]
fn heading_jump_finds_the_heading_in_the_laid_out_reader() {
    use iced::advanced::{
        renderer::Headless,
        widget::{Operation, operation},
    };
    let mut app = app();
    act(
        &mut app,
        Action::Open {
            path: path("/home/demo/project/README.md"),
            repo: None,
        },
    );
    let view = app.views.get_mut("demo").unwrap();
    let file = view.files.reader.as_mut().unwrap();
    file.text = format!(
        "# Start\n\n{}\n\n# Destination\n",
        "paragraph\n\n".repeat(50)
    );
    file.revision += 1;
    view.sync_reader();
    let items = view
        .reader
        .as_ref()
        .unwrap()
        .markdown
        .as_ref()
        .unwrap()
        .items();
    let index = heading_index(items, "destination").unwrap();
    let settings = crate::snapshots::settings();
    let mut renderer = iced::futures::executor::block_on(<iced::Renderer as Headless>::new(
        settings.default_font,
        settings.default_text_size,
        None,
    ))
    .unwrap();
    let mut ui = iced_runtime::UserInterface::build(
        app.view(),
        iced::Size::new(1000.0, 860.0),
        iced_runtime::user_interface::Cache::default(),
        &mut renderer,
    );
    let mut find = FindHeading {
        target: iced::advanced::widget::Id::from(format!("markdown-heading-{index}")),
        session: "demo".into(),
        key: "test".into(),
        origin: 0.0,
        y: None,
    };
    ui.operate(&renderer, &mut operation::black_box(&mut find));
    assert!(
        matches!(find.finish(), operation::Outcome::Some(Message::Files(Action::AnchorOffset { y, .. })) if y > 860.0)
    );
}
