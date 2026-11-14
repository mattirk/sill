//! The tree for each session: refreshes, late and background replies, the drawer following
//! repository changes, and the folder picker.
use super::*;

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

#[test]
fn follow_reports_changes_appearing_and_going_but_not_a_first_sight() {
    let (mut drawer, _) = Drawer::new(None);
    let mut files = files::FilesState::default();
    let pin = path("/repo");
    files.roots.push(files::Root {
        path: pin.clone(),
        global: false,
        metadata: Some(metadata(true)),
        error: None,
    });
    let entry = sill_proto::discovery::StatusEntry {
        path: path("/repo/file"),
        status: " M".into(),
    };
    files.repos.insert(
        key(&pin).into(),
        files::Repo {
            root: Some(pin.clone()),
            read: true,
            entries: vec![entry.clone()],
            ..Default::default()
        },
    );
    assert_eq!(
        drawer.follow("a", &mut files, true).1,
        None,
        "changes already there did not appear"
    );
    files.repos.get_mut(key(&pin)).unwrap().entries.clear();
    assert_eq!(drawer.follow("a", &mut files, true).1, Some(false));
    assert_eq!(drawer.follow("a", &mut files, true).1, None, "unchanged");
    files.repos.get_mut(key(&pin)).unwrap().entries.push(entry.clone());
    assert_eq!(drawer.follow("a", &mut files, true).1, Some(true));
    files.repos.get_mut(key(&pin)).unwrap().entries.push(entry);
    assert_eq!(
        drawer.follow("a", &mut files, true).1,
        None,
        "more changes in a dirty repository are no turn"
    );
}

#[test]
fn changes_bring_the_drawer_out_of_full_and_their_end_puts_it_back() {
    use crate::layout::{Action as Layout, Mode};
    let mut app = app();
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    assert!(!app.drawer.shown);
    let _ = app.follow_drawer("demo", Some(true));
    assert_eq!(app.layout.mode, Mode::Drawer);
    assert!(app.drawer.shown, "changes show the drawer from Full");
    let _ = app.follow_drawer("demo", Some(false));
    assert_eq!(app.layout.mode, Mode::Full, "and Full comes back after them");
    assert!(!app.drawer.shown);

    // a drawer the person had out stays out
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Drawer)));
    let _ = app.follow_drawer("demo", Some(true));
    let _ = app.follow_drawer("demo", Some(false));
    assert_eq!(app.layout.mode, Mode::Drawer);
    assert!(app.drawer.shown);

    // a mode chosen while the changes were there is kept
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    let _ = app.follow_drawer("demo", Some(true));
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Drawer)));
    let _ = app.follow_drawer("demo", Some(false));
    assert_eq!(app.layout.mode, Mode::Drawer, "the person's own choice holds");

    // a background session and the Shelf do not move the window
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    let _ = app.follow_drawer("b", Some(true));
    assert_eq!(app.layout.mode, Mode::Full);
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Shelf)));
    let _ = app.follow_drawer("demo", Some(true));
    assert_eq!(app.layout.mode, Mode::Shelf);
    assert!(!app.drawer.shown);
}

/// Crosscheck, 2026-11-14: the changes went while another session was shown, and the window
/// stayed in Drawer mode for good.
#[test]
fn the_return_to_full_waits_for_the_person_to_come_back_to_the_session() {
    use crate::layout::{Action as Layout, Mode};
    let mut app = app();
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    let _ = app.follow_drawer("demo", Some(true));
    assert_eq!(app.layout.mode, Mode::Drawer);
    let _ = app.update(Message::Choose("b".into()));
    let _ = app.follow_drawer("demo", Some(false));
    assert_eq!(app.layout.mode, Mode::Drawer, "another session is shown: not now");
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(app.layout.mode, Mode::Full, "back in the session: Full again");
    assert!(!app.drawer.shown);

    // still changed on coming back: the drawer stays until the changes go
    let _ = app.follow_drawer("demo", Some(true));
    app.drawer
        .follow_seen
        .insert("demo".into(), (String::new(), true));
    let _ = app.update(Message::Choose("b".into()));
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(app.layout.mode, Mode::Drawer);
    app.drawer.follow_seen.remove("demo");
    let _ = app.follow_drawer("demo", Some(false));
    assert_eq!(app.layout.mode, Mode::Full);

    // a mode chosen in the other session meanwhile is kept
    let _ = app.follow_drawer("demo", Some(true));
    let _ = app.update(Message::Choose("b".into()));
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Full)));
    let _ = app.update(Message::Layout(Layout::Mode(Mode::Drawer)));
    let _ = app.follow_drawer("demo", Some(false));
    let _ = app.update(Message::Choose("demo".into()));
    assert_eq!(app.layout.mode, Mode::Drawer);
}

#[test]
fn changes_show_and_put_away_the_drawer_in_a_plain_window() {
    let mut app = app();
    app.layout.plain = true;
    app.layout.mode = crate::layout::Mode::Drawer;
    app.drawer.shown = false;
    let _ = app.follow_drawer("demo", Some(true));
    assert!(app.drawer.shown);
    let _ = app.follow_drawer("demo", Some(false));
    assert!(!app.drawer.shown);
    // shown by the person, then changes come and go: it stays
    act(&mut app, Action::ToggleDrawer);
    let _ = app.follow_drawer("demo", Some(true));
    let _ = app.follow_drawer("demo", Some(false));
    assert!(app.drawer.shown);
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
