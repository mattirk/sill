//! The window's side of workspaces (docs/parallel-coding.md §12): what the shelf offers, the pin
//! chips' marks, and the requests Apply and Discard make with the revision the person saw.
use serde_json::{Value, json};
use silli_client::workspaces::Loaded;
use silli_proto::method;

use super::*;
use crate::tests::{daemon, live_app, settle_task};

fn path(p: &str) -> Value {
    json!(silli_proto::sessions::HostPath::from_bytes(p.as_bytes()))
}

fn workspace() -> Value {
    json!({"workspace_id": "ws-1", "name": "repo", "session_id": "demo", "source": path("/home/me/repo"),
           "source_branch": "main", "base": "b0", "branch": "silli/repo-ws-1", "path": path("/ws/repo-ws-1"),
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
            "live": [{"source": path("/home/me/sub"), "reason": "submodules", "message": "Live: Repositories with submodules are not supported yet."}]}))
        .unwrap(),
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
    let repo: silli_proto::sessions::HostPath = serde_json::from_value(path("/home/me/repo")).unwrap();
    let (_, said) = crate::workspaces_ui::chip_mark(view, &repo, "files");
    assert!(said.unwrap().contains("branch silli/repo-ws-1"));
    let sub: silli_proto::sessions::HostPath = serde_json::from_value(path("/home/me/sub")).unwrap();
    let (_, said) = crate::workspaces_ui::chip_mark(view, &sub, "files");
    assert!(said.unwrap().starts_with("Live:"));
    let other: silli_proto::sessions::HostPath = serde_json::from_value(path("/home/me/other")).unwrap();
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
        let get = daemon.expect(method::WORKSPACES_GET);
        assert_eq!(get["params"]["workspace_id"], "ws-1");
        daemon.reply(&get, detail(3));
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        assert_eq!(apply["params"]["revision"], revision());
        assert!(!apply["params"]["operation_id"].as_str().unwrap().is_empty());
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        let discard = daemon.expect(method::WORKSPACES_DISCARD);
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
    conflict.details[0].workspace.operation = Some(serde_json::from_value(json!({"kind": "rebase", "state": "conflict", "detail": stopped})).unwrap());
    // the demo's own session is in a turn: the agent is not asked then
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(conflict.clone());
    let task = app.update(Message::Workspaces(Event::Resolve("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(conflict);
    idle.draft = crate::editor::Editor::with_text("half a thought");
    app.views.insert("idle".into(), idle);
    let _task = app.update(Message::Workspaces(Event::Resolve("idle".into(), "ws-1".into())));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
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
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        daemon.refuse_for(
            &apply,
            "uncommitted",
            "The workspace has uncommitted changes: apply from its session, where the agent is asked to commit them first.",
        );
        let submit = daemon.expect(method::SESSIONS_SUBMIT);
        assert_eq!(submit["params"]["session_id"], "idle");
        let text = submit["params"]["text"].as_str().unwrap().to_string();
        daemon.reply(&submit, json!({}));
        // after the turn: applied at the agent's commit
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
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
        let get = daemon.expect(method::WORKSPACES_GET);
        let mut d = detail(3);
        d["revision"]["destination_branch"] = json!("other");
        daemon.reply(&get, d);
        let apply = daemon.expect(method::WORKSPACES_APPLY);
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
        let discard = daemon.expect(method::WORKSPACES_DISCARD);
        assert_eq!(discard["params"]["revision"]["tip"], "t2");
        daemon.reply(&discard, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        // one Apply, likewise
        let get = daemon.expect(method::WORKSPACES_GET);
        let mut d = detail(3);
        d["revision"]["tip"] = json!("t4");
        d["revision"]["destination_branch"] = json!("feature");
        daemon.reply(&get, d);
        let apply = daemon.expect(method::WORKSPACES_APPLY);
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
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        daemon.refuse_for(&apply, "uncommitted", "The workspace has uncommitted changes.");
    });
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed("tree-1")))));
    settle_task(&mut app, task);
    server.join().unwrap();
    let state = &app.views["idle"].workspaces;
    assert!(state.committing.is_none() && state.committed.is_none(), "no second commit turn is waited for");
    let (said, bad) = state.notices["ws-1"].clone();
    assert!(bad && said.starts_with("Silli still finds uncommitted changes"), "{said}");
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
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
        let copy = daemon.expect(method::WORKSPACES_COPY);
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

/// Apply as branch with uncommitted work: the agent is asked to commit, and at its turn's end the
/// branch follows (not an Apply), named by the daemon.
#[test]
fn apply_as_branch_asks_for_the_commit_then_makes_the_branch() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    app.views.insert("idle".into(), idle);
    let server = std::thread::spawn(move || {
        let first = daemon.expect(method::WORKSPACES_BRANCH);
        daemon.refuse_for(&first, "uncommitted", "The workspace has uncommitted changes.");
        let submit = daemon.expect(method::SESSIONS_SUBMIT);
        assert_eq!(submit["params"]["purpose"], "commit");
        daemon.reply(&submit, json!({}));
        let second = daemon.expect(method::WORKSPACES_BRANCH);
        assert_eq!(second["params"]["revision"]["tip"], "t2", "the committed work");
        daemon.reply(&second, json!({"workspace": workspace(), "operation": null, "branch": "fix_the_clock"}));
    });
    let task = app.update(Message::Workspaces(Event::Branch("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["idle"].workspaces;
    assert!(state.branch_after_commit && state.committing.as_ref().is_some_and(|(w, _)| w == "ws-1"), "waiting for the agent's commit");
    let tree = state.committing.as_ref().unwrap().1.clone();
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed(&tree)))));
    settle_task(&mut app, task);
    server.join().unwrap();
    let state = &app.views["idle"].workspaces;
    assert!(!state.branch_after_commit && state.committing.is_none() && state.committed.is_none());
    let (said, bad) = state.notices["ws-1"].clone();
    assert!(!bad && said.starts_with("Branch fix_the_clock is in your checkout"), "{said}");
}

/// Crosscheck, 2026-11-16: a branch's commit turn that left work uncommitted left the window set
/// to make a branch after the next commit turn, so a later Apply from Changes made a branch. The
/// turn decides once what follows it; a commit asked for by an Apply is followed by an Apply.
#[test]
fn a_failed_commit_for_a_branch_leaves_no_branch_for_a_later_apply() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    app.views.insert("idle".into(), idle);
    let server = std::thread::spawn(move || {
        let first = daemon.expect(method::WORKSPACES_BRANCH);
        daemon.refuse_for(&first, "uncommitted", "The workspace has uncommitted changes.");
        let submit = daemon.expect(method::SESSIONS_SUBMIT);
        daemon.reply(&submit, json!({}));
        // the Changes page's Apply asks for a commit of its own
        let submit = daemon.expect(method::SESSIONS_SUBMIT);
        daemon.reply(&submit, json!({}));
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
    });
    let task = app.update(Message::Workspaces(Event::Branch("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    assert!(app.views["idle"].workspaces.branch_after_commit);
    // the agent's turn ends with the work still uncommitted
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(uncommitted()))));
    settle_task(&mut app, task);
    assert!(!app.views["idle"].workspaces.branch_after_commit, "nothing waits on a turn that is over");
    // later, Apply from Changes goes straight to a commit turn
    let task = app.update(Message::Workspaces(Event::Commit("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    assert!(!app.views["idle"].workspaces.branch_after_commit);
    let tree = app.views["idle"].workspaces.committing.as_ref().unwrap().1.clone();
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed(&tree)))));
    settle_task(&mut app, task);
    server.join().unwrap();
    assert_eq!(app.views["idle"].workspaces.notices.get("ws-1"), Some(&("Applied to your checkout.".to_string(), false)), "an Apply, not a branch");
}

/// The daemon's side of a session closing as its tab's × closes it.
fn expect_close(daemon: &mut crate::tests::Daemon, session: &str) {
    let end = daemon.expect(method::SESSIONS_END);
    assert_eq!(end["params"], json!({"session_id": session, "reason": "close"}));
    daemon.reply(&end, json!({"pending": false, "deleted": false}));
    let list = daemon.expect(method::SESSIONS_LIST);
    daemon.reply(&list, json!({"sessions": [], "next": null}));
}

/// Apply and close (the caret's menu, 2026-11-20): the Apply of what the person saw, and once
/// the work is in the checkout the session closes as its tab's × closes it.
#[test]
fn apply_and_close_applies_then_closes_the_session() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(loaded(3));
    let _ = app.update(Message::Workspaces(Event::Menu("demo".into(), "ws-1".into())));
    let server = std::thread::spawn(move || {
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, detail(3));
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        assert_eq!(apply["params"]["revision"], revision());
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        expect_close(&mut daemon, "demo");
    });
    let task = app.update(Message::Workspaces(Event::ApplyClose("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    server.join().unwrap();
    assert!(app.tabs.is_empty() && app.selected.is_none(), "the session's tab went with it");
}

/// With uncommitted work the agent is asked to commit first, as for Apply; the Apply after its
/// turn closes the session, as the person asked at the click.
#[test]
fn apply_and_close_waits_for_the_agents_commit() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut idle = crate::SessionView::new("idle");
    idle.workspaces.loaded = Some(uncommitted());
    app.views.insert("idle".into(), idle);
    let server = std::thread::spawn(move || {
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        daemon.refuse_for(&apply, "uncommitted", "The workspace has uncommitted changes.");
        let submit = daemon.expect(method::SESSIONS_SUBMIT);
        assert_eq!(submit["params"]["purpose"], "commit");
        daemon.reply(&submit, json!({}));
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        assert_eq!(apply["params"]["revision"]["tip"], "t2", "the committed work");
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        expect_close(&mut daemon, "idle");
    });
    let task = app.update(Message::Workspaces(Event::ApplyClose("idle".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["idle"].workspaces;
    assert!(state.close_after_commit && state.committing.as_ref().is_some_and(|(w, _)| w == "ws-1"), "waiting for the agent's commit");
    let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed("tree-1")))));
    settle_task(&mut app, task);
    server.join().unwrap();
    assert!(app.views.get("idle").is_none_or(|v| !v.workspaces.close_after_commit && v.failed.is_none()));
}

/// What stops the Apply leaves the session open, and nothing closes it later: a commit turn that
/// left work uncommitted takes the close with it, so the plain Apply that follows the next commit
/// turn only applies. So does a turn during which the person went on writing in the session.
#[test]
fn apply_and_close_closes_nothing_when_the_apply_did_not_go_as_asked() {
    for writing in [false, true] {
        let (client, mut daemon) = daemon();
        let mut app = live_app(vec![client]);
        let mut idle = crate::SessionView::new("idle");
        idle.workspaces.loaded = Some(uncommitted());
        app.views.insert("idle".into(), idle);
        let server = std::thread::spawn(move || {
            let get = daemon.expect(method::WORKSPACES_GET);
            daemon.reply(&get, serde_json::to_value(&uncommitted().details[0]).unwrap());
            let apply = daemon.expect(method::WORKSPACES_APPLY);
            daemon.refuse_for(&apply, "uncommitted", "The workspace has uncommitted changes.");
            let submit = daemon.expect(method::SESSIONS_SUBMIT);
            daemon.reply(&submit, json!({}));
            if !writing {
                // the Changes page's Apply asks for a commit of its own
                let submit = daemon.expect(method::SESSIONS_SUBMIT);
                daemon.reply(&submit, json!({}));
            }
            let get = daemon.expect(method::WORKSPACES_GET);
            daemon.reply(&get, serde_json::to_value(&committed("tree-1").details[0]).unwrap());
            let apply = daemon.expect(method::WORKSPACES_APPLY);
            daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        });
        let task = app.update(Message::Workspaces(Event::ApplyClose("idle".into(), "ws-1".into())));
        settle_task(&mut app, task);
        assert!(app.views["idle"].workspaces.close_after_commit);
        if writing {
            app.views.get_mut("idle").unwrap().draft = crate::editor::Editor::with_text("one more thing");
        } else {
            // the agent's turn ends with the work still uncommitted
            let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(uncommitted()))));
            settle_task(&mut app, task);
            let state = &app.views["idle"].workspaces;
            assert!(!state.close_after_commit, "nothing waits on a turn that is over");
            assert_eq!(state.notices.get("ws-1"), Some(&("The agent left changes uncommitted, so nothing was applied.".to_string(), true)));
            let task = app.update(Message::Workspaces(Event::Commit("idle".into(), "ws-1".into())));
            settle_task(&mut app, task);
            assert!(!app.views["idle"].workspaces.close_after_commit);
        }
        let tree = app.views["idle"].workspaces.committing.as_ref().unwrap().1.clone();
        let task = app.update(Message::Workspaces(Event::Committed("idle".into(), Ok(committed(&tree)))));
        settle_task(&mut app, task);
        server.join().unwrap();
        let view = &app.views["idle"];
        assert_eq!(view.workspaces.notices.get("ws-1"), Some(&("Applied to your checkout.".to_string(), false)), "applied");
        assert!(view.failed.is_none() && app.list_error.is_none(), "and no close was asked for: {:?} {:?}", view.failed, app.list_error);
        if writing {
            assert_eq!(view.draft.text().trim_end(), "one more thing", "what the person wrote is still there");
        }
    }
}

/// A session with two repositories: Apply and close on one applies it and leaves the session
/// open while the other still has work to apply, and the other's line says so.
#[test]
fn apply_and_close_keeps_the_session_while_another_repository_has_work() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut two = loaded(3);
    let mut other = detail(2);
    other["workspace"]["workspace_id"] = json!("ws-2");
    other["workspace"]["source"] = path("/home/me/tools");
    two.details.push(serde_json::from_value(other).unwrap());
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(two);
    let server = std::thread::spawn(move || {
        let get = daemon.expect(method::WORKSPACES_GET);
        daemon.reply(&get, detail(3));
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
    });
    let task = app.update(Message::Workspaces(Event::ApplyClose("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    server.join().unwrap();
    let view = &app.views["demo"];
    assert!(view.failed.is_none() && app.list_error.is_none() && !app.tabs.is_empty(), "the session is still open");
    let (said, bad) = view.workspaces.notices["ws-2"].clone();
    assert!(!bad && said == "repo is applied. The session stays open: this still has work to apply.", "{said}");
}

/// A checkout on another branch than the workspace started from is asked about once, as for
/// Apply: the first click on Apply and close sends nothing and leaves its menu open with the
/// question on its label; the second applies onto that branch and closes.
#[test]
fn apply_and_close_onto_another_branch_asks_first() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let mut on_feature = loaded(3);
    on_feature.details[0].revision.destination_branch = "feature".into();
    app.views.get_mut("demo").unwrap().workspaces.loaded = Some(on_feature);
    // the turn has ended: the shelf is there to click
    let conversation = &mut app.views.get_mut("demo").unwrap().conversation;
    if let Some(session) = &mut conversation.session {
        session.activity = "idle".into();
        session.turn_id = None;
    }
    let _ = app.update(Message::Workspaces(Event::Menu("demo".into(), "ws-1".into())));
    {
        let mut ui = crate::tests::simulator(&app, (720.0, 860.0));
        ui.click("Apply and close").unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(&messages[..], [Message::Workspaces(Event::ApplyClose(s, w))] if s == "demo" && w == "ws-1"), "{messages:?}");
    }
    let task = app.update(Message::Workspaces(Event::ApplyClose("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    let state = &app.views["demo"].workspaces;
    assert!(state.notices["ws-1"].0.contains("started from main, but your checkout is on feature"));
    assert!(state.running.is_none() && state.menu.as_deref() == Some("ws-1"), "asked, with the menu still open");
    crate::tests::simulator(&app, (720.0, 860.0)).find("Apply onto feature and close?").unwrap();
    let server = std::thread::spawn(move || {
        let get = daemon.expect(method::WORKSPACES_GET);
        let mut d = detail(3);
        d["revision"]["destination_branch"] = json!("feature");
        daemon.reply(&get, d);
        let apply = daemon.expect(method::WORKSPACES_APPLY);
        assert_eq!(apply["params"]["revision"]["destination_branch"], "feature");
        daemon.reply(&apply, json!({"workspace": workspace(), "operation": null, "discarded": null}));
        expect_close(&mut daemon, "demo");
    });
    let task = app.update(Message::Workspaces(Event::ApplyClose("demo".into(), "ws-1".into())));
    settle_task(&mut app, task);
    server.join().unwrap();
    assert!(app.tabs.is_empty(), "applied onto feature, and closed");
}
