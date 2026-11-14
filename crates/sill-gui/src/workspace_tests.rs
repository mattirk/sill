//! Files in the window: each session keeps its own folders and reader across refreshes and tab
//! switches, and a reply only changes the session it was asked for.
use super::*;
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
    let _ = app.update(Message::Workspace(action));
}

/// A request for the demo session made by hand, so its reply can come late; its serial.
fn pending(app: &mut App, session: &str, request: Request) -> (u64, Request) {
    app.workspace.serial += 1;
    let serial = app.workspace.serial;
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
    assert!(app.workspace.shown);
    let browsed = path("/home/demo/elsewhere");
    let (serial, request) = pending(&mut app, "demo", Request::Inspect(browsed.clone()));
    let _ = app.update(Message::Choose("b".into()));
    act(&mut app, Action::ToggleDrawer);
    assert!(!app.workspace.shown);
    reply(
        &mut app,
        "demo",
        serial,
        request,
        Ok(Data::Metadata(metadata(true))),
    );
    assert!(
        !app.workspace.shown,
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
    assert!(!app.workspace.shown);
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
    let (mut workspace, _) = Workspace::new(None);
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
    workspace.follow("a", &mut files, true);
    assert!(!files.diff_mode, "a clean repository selects Files");
    files.diff_mode = true;
    workspace.follow("a", &mut files, true);
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
    workspace.follow("a", &mut files, true);
    assert!(files.diff_mode, "a dirty transition selects Diff");
    files.repos.get_mut(key(&pin)).unwrap().entries.clear();
    workspace.follow("a", &mut files, true);
    assert!(!files.diff_mode, "cleaning the repository restores Files");
    files.diff_mode = true;
    workspace.follow("a", &mut files, false);
    assert!(files.diff_mode, "disabled leaves the manual view alone");
    workspace.follow("a", &mut files, true);
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
    assert!(app.workspace.pending_discard.is_some());
    act(&mut app, Action::CancelDiscard);
    assert!(app.workspace.pending_discard.is_none());
    act(&mut app, Action::RefreshFile);
    assert_eq!(
        app.views["demo"].reader.as_ref().unwrap().content.text(),
        edited
    );
    act(&mut app, Action::CancelDiscard);
    let _ = app.update(Message::CloseRequested);
    assert!(app.workspace.pending_discard.is_some());
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
        r.content = text_editor::Content::with_text(&r.saved);
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
    let task = app.update(Message::Workspace(Action::SaveFile));
    settle_task(&mut app, task);
    assert!(!app.views["demo"].reader.as_ref().unwrap().dirty);
    assert_eq!(
        app.views["demo"].files.reader.as_ref().unwrap().text,
        expected
    );
    insert(&mut app, 'Y');
    let second = app.views["demo"].reader.as_ref().unwrap().content.text();
    let task = app.update(Message::Workspace(Action::SaveFile));
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
