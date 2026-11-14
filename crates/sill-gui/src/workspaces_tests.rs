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
    assert!(app.views["demo"].workspaces.armed.get().is_some_and(|(w, _)| w == "ws-1"));
    let task = app.update(Message::Workspaces(Event::Discard("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let (said, bad) = app.views["demo"].workspaces.notices["ws-1"].clone();
    assert!(bad && said.starts_with("The workspace changed since you looked"), "{said}");
    server.join().unwrap();
}

#[test]
fn resolve_asks_the_agent_to_finish_a_stopped_rebase() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let stopped = "The rebase onto main (3335b133341e) stopped: src/drawer.rs";
    let mut conflict = loaded(3);
    conflict.details[0].workspace.operation =
        Some(serde_json::from_value(json!({"kind": "rebase", "state": "conflict", "detail": stopped})).unwrap());
    // the demo's own session is in a turn: the agent is not asked then
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(conflict.clone());
    let task = app.update(Message::Workspaces(Event::Resolve("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(conflict);
    idle.draft = crate::editor::Editor::with_text("half a thought");
    app.views.insert("idle".into(), idle);
    let _task = app.update(Message::Workspaces(Event::Resolve("idle".into(), "ws-1".into())));
    let request = daemon.expect("_sill/sessions.submit");
    assert_eq!(request["params"]["session_id"], "idle");
    let text = request["params"]["text"].as_str().unwrap();
    assert!(text.starts_with(stopped), "{text}");
    assert!(text.contains("git rebase --continue") && text.contains("Do not abort"), "{text}");
    daemon.reply(&request, json!({}));
    assert_eq!(app.views["idle"].draft.text().trim_end(), "half a thought", "the composer keeps what the person was writing");
    // a workspace without a stopped rebase has nothing to resolve
    app.views.get_mut("idle").unwrap().workspaces.loaded = Some(loaded(3));
    let task = app.update(Message::Workspaces(Event::Resolve("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
}

/// The workspace as the person sees it with work the agent has not committed, and after the
/// agent committed exactly that.
fn uncommitted() -> Loaded {
    let mut l = loaded(3);
    l.details[0].summary.uncommitted = vec!["src/a.rs".into()];
    l.details[0].state_tree = "tree-1".into();
    l
}

fn committed(state_tree: &str) -> Loaded {
    let mut l = loaded(3);
    l.details[0].revision.tip = "t2".into();
    l.details[0].revision.worktree = String::new();
    l.details[0].state_tree = state_tree.into();
    l
}

/// Apply brings commits only: with uncommitted changes in the workspace the click asks the
/// session's agent to commit them, and the Apply follows at its turn's end when the same work is
/// all committed.
#[test]
fn apply_with_uncommitted_changes_asks_the_agent_to_commit_and_applies_after_its_turn() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    idle.draft = crate::editor::Editor::with_text("half a thought");
    app.views.insert("idle".into(), idle);
    let server = std::thread::spawn(move || {
        let get = daemon.expect("_sill/workspaces.get");
        daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
        let apply = daemon.expect("_sill/workspaces.apply");
        daemon.refuse_for(&apply, "uncommitted", "The workspace has uncommitted changes: apply from its session, where the agent is asked to commit them first.");
        let submit = daemon.expect("_sill/sessions.submit");
        assert_eq!(submit["params"]["session_id"], "idle");
        let text = submit["params"]["text"].as_str().unwrap().to_string();
        daemon.reply(&submit, json!({}));
        // after the turn: applied at the agent's commit
        let get = daemon.expect("_sill/workspaces.get");
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect("_sill/workspaces.apply");
        assert_eq!(apply["params"]["revision"]["tip"], "t2");
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        text
    });
    let task = app.update(Message::Workspaces(Event::Apply("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["idle"].workspaces;
    assert_eq!(state.committing, Some(("ws-1".to_string(), "tree-1".to_string())), "waiting for the agent's commit");
    assert!(state.running.is_none() && state.notices.is_empty());
    assert_eq!(app.views["idle"].draft.text().trim_end(), "half a thought", "the composer keeps what the person was writing");
    // the turn ends with exactly that work committed
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed("tree-1")))));
    settle_task(&mut app, task);
    let state = &app.views["idle"].workspaces;
    assert!(state.committing.is_none());
    assert_eq!(state.notices.get("ws-1"), Some(&("Applied to your checkout.".to_string(), false)));
    let text = server.join().unwrap();
    assert!(text.contains("/home/me/repo") && text.contains("commit message of your own") && text.contains("Change nothing else"), "{text}");
}

/// What the person saw is what is applied: a turn that left changes uncommitted, or changed the
/// work, applies nothing and says so.
#[test]
fn a_commit_turn_that_changed_the_work_applies_nothing() {
    let (client, _daemon) = daemon();
    let mut app = live_app(vec![client]);
    for (after, said) in [
        (uncommitted(), "The agent left changes uncommitted, so nothing was applied."),
        (committed("tree-2"), "The work changed while the agent committed it: look again, then apply."),
    ] {
        let mut idle = crate::SessionView::new("idle");
        idle.workspaces.loaded = Some(uncommitted());
        idle.workspaces.committing = Some(("ws-1".into(), "tree-1".into()));
        app.views.insert("idle".into(), idle);
        let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(after))));
        settle_task(&mut app, task);
        let state = &app.views["idle"].workspaces;
        assert!(state.committing.is_none() && state.running.is_none());
        assert_eq!(state.notices.get("ws-1"), Some(&(said.to_string(), true)));
    }
    // no Apply was waiting: a turn's end is only a turn's end
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed("tree-1")))));
    settle_task(&mut app, task);
    assert_eq!(app.views["idle"].workspaces.notices.len(), 1);
}

#[test]
fn apply_onto_another_branch_asks_once_for_that_branch() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let on = |branch: &str| {
        let mut l = loaded(3);
        l.details[0].revision.destination_branch = branch.into();
        l
    };
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(on("feature"));
    // the first click asks and sends nothing
    let task = app.update(Message::Workspaces(Event::Apply("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["demo"].workspaces;
    assert!(state.apply_armed.get().is_some_and(|(w, seen)| w == "ws-1" && seen.destination_branch == "feature"));
    assert!(state.notices["ws-1"].0.contains("started from main, but your checkout is on feature"));
    assert!(state.running.is_none());
    // the checkout switched again before the second click: asked again, for the new branch
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(on("other"));
    let task = app.update(Message::Workspaces(Event::Apply("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    assert!(app.views["demo"].workspaces.apply_armed.get().is_some_and(|(w, seen)| w == "ws-1" && seen.destination_branch == "other"));
    // clicking elsewhere drops the question
    let _ = app.update(Message::Workspaces(Event::Disarm("demo".into())));
    assert_eq!(app.views["demo"].workspaces.apply_armed.get(), None);
    let task = app.update(Message::Workspaces(Event::Apply("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    // the second click for the same branch applies onto it
    let server = std::thread::spawn(move || {
        let get = daemon.expect("_sill/workspaces.get");
        let mut d = detail(3);
        d["revision"]["destination_branch"] = json!("other");
        daemon.reply(&get, d);
        let apply = daemon.expect("_sill/workspaces.apply");
        assert_eq!(apply["params"]["revision"]["destination_branch"], "other");
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
    });
    let task = app.update(Message::Workspaces(Event::Apply("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    server.join().unwrap();
    let state = &app.views["demo"].workspaces;
    assert_eq!(state.notices.get("ws-1"), Some(&("Applied to your checkout.".to_string(), false)));
    assert_eq!(state.apply_armed.get(), None);
    assert_eq!(state.apply_onto, None);
}

/// The second click confirms what the shelf showed at the first. A workspace read again between
/// the two with other work in it is asked about again: nothing is sent for what the person has
/// not seen.
#[test]
fn a_workspace_that_changed_between_the_two_clicks_is_asked_about_again() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let at = |tip: &str, branch: &str| {
        let mut l = loaded(3);
        l.details[0].revision.tip = tip.into();
        l.details[0].revision.destination_branch = branch.into();
        l
    };
    let armed_for = |app: &App, tip: &str| app.views["demo"].workspaces.armed.get().is_some_and(|(w, seen)| w == "ws-1" && seen.tip == tip);
    let click = |app: &mut App, event: Event| {
        let task = app.update(Message::Workspaces(event));
        settle_task(app, task);
    };
    let server = std::thread::spawn(move || {
        // one Discard, of the work shown at the click before it
        let discard = daemon.expect("_sill/workspaces.discard");
        assert_eq!(discard["params"]["revision"]["tip"], "t2");
        daemon.reply(&discard, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        // one Apply, likewise
        let get = daemon.expect("_sill/workspaces.get");
        let mut d = detail(3);
        d["revision"]["tip"] = json!("t4");
        d["revision"]["destination_branch"] = json!("feature");
        daemon.reply(&get, d);
        let apply = daemon.expect("_sill/workspaces.apply");
        assert_eq!(apply["params"]["revision"]["tip"], "t4");
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
    });
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(at("t1", "main"));
    click(&mut app, Event::Discard("demo".into(), "ws-1".into()));
    assert!(armed_for(&app, "t1"));
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(at("t2", "main"));
    click(&mut app, Event::Discard("demo".into(), "ws-1".into()));
    assert!(armed_for(&app, "t2"), "armed again, for what the shelf shows now");
    assert!(app.views["demo"].workspaces.running.is_none() && app.views["demo"].workspaces.notices.is_empty());
    click(&mut app, Event::Discard("demo".into(), "ws-1".into()));
    assert_eq!(app.views["demo"].workspaces.armed.get(), None);
    assert_eq!(app.views["demo"].workspaces.notices["ws-1"], ("Discarded. Undo on Changes for 7 days.".to_string(), false));
    // an Apply onto another branch
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(at("t3", "feature"));
    click(&mut app, Event::Apply("demo".into(), "ws-1".into()));
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(at("t4", "feature"));
    click(&mut app, Event::Apply("demo".into(), "ws-1".into()));
    let state = &app.views["demo"].workspaces;
    assert!(state.apply_armed.get().is_some_and(|(_, seen)| seen.tip == "t4") && state.apply_onto.is_none() && state.running.is_none());
    assert!(state.notices["ws-1"].0.contains("your checkout is on feature"));
    click(&mut app, Event::Apply("demo".into(), "ws-1".into()));
    server.join().unwrap();
    assert_eq!(app.views["demo"].workspaces.notices["ws-1"], ("Applied to your checkout.".to_string(), false));
}

/// The Apply loop of 2026-11-11: the daemon went on refusing as `uncommitted` although the
/// agent's turn had left everything committed, and each Apply sent the agent another commit
/// prompt. One commit turn for an Apply: the second refusal is said, not asked again.
#[test]
fn an_apply_asks_the_agent_to_commit_once() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    idle.workspaces.committing = Some(("ws-1".into(), "tree-1".into()));
    app.views.insert("idle".into(), idle);
    let server = std::thread::spawn(move || {
        let get = daemon.expect("_sill/workspaces.get");
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect("_sill/workspaces.apply");
        daemon.refuse_for(&apply, "uncommitted", "The workspace has uncommitted changes.");
    });
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed("tree-1")))));
    settle_task(&mut app, task);
    server.join().unwrap();
    let state = &app.views["idle"].workspaces;
    assert!(state.committing.is_none() && state.committed.is_none(), "no second commit turn is waited for");
    let (said, bad) = state.notices["ws-1"].clone();
    assert!(bad && said.starts_with("Sill still finds uncommitted changes"), "{said}");
}

/// The caret beside Apply opens "Apply without commit": the workspace's state as it is, into the
/// checkout uncommitted, with no commit asked of the agent.
#[test]
fn apply_without_commit_copies_what_the_person_saw() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    app.views.insert("idle".into(), idle);
    let _ = app.update(Message::Workspaces(Event::Menu("idle".into(), "ws-1".into())));
    assert_eq!(app.views["idle"].workspaces.menu.as_deref(), Some("ws-1"));
    assert!(app.workspace_shelf(&app.views["idle"]).is_some());
    let seen = serde_json::to_value(&uncommitted().details[0].revision).unwrap();
    let server = std::thread::spawn(move || {
        let get = daemon.expect("_sill/workspaces.get");
        daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
        let copy = daemon.expect("_sill/workspaces.copy");
        assert_eq!(copy["params"]["revision"], seen);
        daemon.reply(&copy, json!({"workspace": workspace(), "operation": null, "discarded": null}));
    });
    let task = app.update(Message::Workspaces(Event::Copy("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    server.join().unwrap();
    let state = &app.views["idle"].workspaces;
    assert!(state.menu.is_none() && state.committing.is_none());
    assert_eq!(state.notices.get("ws-1"), Some(&("Written into your checkout, uncommitted. The work is still here to apply.".to_string(), false)));
}
