//! The window's side of workspaces (docs/parallel-coding.md §12): what the shelf offers, the pin
//! chips' marks, and the requests Apply and Discard make with the revision the person saw.
use serde_json::{Value, json};
use sill_client::workspaces::Loaded;

use super::*;
use crate::tests::{daemon, live_app, settle_task};

fn path(p: &str) -> Value {
    json!(sill_proto::sessions::HostPath::from_path(std::path::Path::new(p)))
}

fn workspace() -> Value {
    json!({"workspace_id": "ws-1", "name": "repo", "session_id": "demo", "source": path("/home/me/repo"),
           "source_branch": "main", "base": "b0", "branch": "sill/repo-ws-1", "path": path("/ws/repo-ws-1"),
           "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"})
}

fn revision() -> Value {
    json!({"tip": "t1", "worktree": "w1", "destination_branch": "main", "destination_tip": "b0"})
}

fn detail(files: u32) -> Value {
    json!({"workspace": workspace(), "revision": revision(),
           "summary": {"commits": 1, "files": files, "added": "40", "removed": "12", "uncommitted": []},
           "needs_rebase": false})
}

fn loaded(files: u32) -> Loaded {
    Loaded {
        view: serde_json::from_value(json!({"workspaces_mode": true, "workspaces": [workspace()],
            "live": [{"source": path("/home/me/sub"), "reason": "submodules", "message": "Live: Repositories with submodules are not supported yet."}]})).unwrap(),
        details: vec![serde_json::from_value(detail(files)).unwrap()],
    }
}

#[test]
fn the_shelf_offers_work_not_yet_applied_and_the_chips_say_which_is_which() {
    let (mut app, _) = App::new(true, None);
    // an idle session: the demo's own is in the middle of a turn, when the shelf stays away
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(loaded(3));
    app.views.insert("idle".into(), idle);
    let busy = &app.views["demo"];
    assert!(busy.conversation.busy());
    let view = &app.views["idle"];
    assert_eq!(view.workspaces.offered().len(), 1);
    assert!(app.workspace_shelf(view).is_some());
    let repo: sill_proto::sessions::HostPath = serde_json::from_value(path("/home/me/repo")).unwrap();
    let (_, said) = crate::workspaces_ui::chip_mark(view, &repo, "files");
    assert!(said.unwrap().contains("branch sill/repo-ws-1"));
    let sub: sill_proto::sessions::HostPath = serde_json::from_value(path("/home/me/sub")).unwrap();
    let (_, said) = crate::workspaces_ui::chip_mark(view, &sub, "files");
    assert!(said.unwrap().starts_with("Live:"));
    let other: sill_proto::sessions::HostPath = serde_json::from_value(path("/home/me/other")).unwrap();
    assert!(crate::workspaces_ui::chip_mark(view, &other, "files").1.is_none());
    // nothing left to apply: no shelf
    let empty = loaded(0);
    let mut empty = empty;
    empty.details[0].summary.commits = 0;
    empty.details[0].summary.uncommitted = vec!["already-applied.txt".into()];
    app.views.get_mut("idle").unwrap().workspaces.loaded = Some(empty);
    assert!(app.workspace_shelf(&app.views["idle"]).is_none());
}

#[test]
fn apply_and_discard_send_the_revision_the_person_saw() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(loaded(3));
    let server = std::thread::spawn(move || {
        let get = daemon.expect("_sill/workspaces.get");
        assert_eq!(get["params"]["workspace_id"], "ws-1");
        daemon.reply(&get, detail(3));
        let apply = daemon.expect("_sill/workspaces.apply");
        assert_eq!(apply["params"]["revision"], revision());
        assert!(!apply["params"]["operation_id"].as_str().unwrap().is_empty());
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        let discard = daemon.expect("_sill/workspaces.discard");
        assert_eq!(discard["params"]["revision"], revision());
        daemon.refuse(&discard, "invalid_state", "The workspace changed since you looked; look again at what would be discarded.");
    });
    let task = app.update(Message::Workspaces(Event::Apply("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["demo"].workspaces;
    assert_eq!(state.notices.get("ws-1"), Some(&("Applied to your checkout.".to_string(), false)));
    assert!(state.running.is_none());
    // Discard asks twice: the first click only arms it
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(loaded(3));
    let task = app.update(Message::Workspaces(Event::Discard("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    assert_eq!(app.views["demo"].workspaces.armed.as_deref(), Some("ws-1"));
    let task = app.update(Message::Workspaces(Event::Discard("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let (said, bad) = app.views["demo"].workspaces.notices["ws-1"].clone();
    assert!(bad && said.starts_with("The workspace changed since you looked"), "{said}");
    server.join().unwrap();
}
