//! Files in the window: each session keeps its own folders and reader across refreshes and tab
//! switches, and a reply only changes the session it was asked for.
use super::*;
use super::links::{FindHeading, LinkTarget, heading_index, resolve_link};
use iced::widget::markdown;
use sill_client::files::{Data, Request, key};
use sill_proto::admin;

use crate::{App, Message, SessionView, Tab};

mod edits;
mod links;
mod tree;

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
