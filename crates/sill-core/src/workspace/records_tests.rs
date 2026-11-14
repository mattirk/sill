//! §12 records against real repositories, with a test host whose sessions' lives, turns and pins
//! the test sets: workspaces made at a session's start, Apply and Discard during it, Undo, the
//! spare folder, rebase, and what a restart finds.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use rusqlite::Connection;
use sill_proto::workspaces as api;

use super::*;

struct TestHost {
    conn: Mutex<Connection>,
    lives: Mutex<HashMap<String, Life>>,
    busy: Mutex<Vec<String>>,
    pins: Mutex<HashMap<String, Vec<PathBuf>>>,
    root: PathBuf,
    now: Mutex<i64>,
}

impl Host for TestHost {
    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.conn.lock().unwrap())
    }
    fn life(&self, session: &str) -> Life {
        *self.lives.lock().unwrap().get(session).unwrap_or(&Life::Missing)
    }
    fn busy(&self, session: &str) -> bool {
        self.busy.lock().unwrap().iter().any(|s| s == session)
    }
    fn session_pins(&self, session: &str) -> Result<Vec<PathBuf>> {
        Ok(self.pins.lock().unwrap().get(session).cloned().unwrap_or_default())
    }
    /// No jail in a unit test: the command runs in the clone, where the jail would have it.
    fn run_jailed(&self, session: &str, cwd: &Path, argv: &[&str]) -> Result<std::process::Output> {
        let layout = self.db(|c| session_layout(c, session))?;
        let dir = layout.iter().find(|l| cwd.starts_with(&l.source)).map(|l| l.clone.join(cwd.strip_prefix(&l.source).unwrap())).unwrap();
        Ok(Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()?)
    }
    fn root(&self) -> PathBuf {
        self.root.clone()
    }
    fn now(&self) -> i64 {
        *self.now.lock().unwrap()
    }
}

struct Fixture {
    dir: PathBuf,
    host: TestHost,
    source: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main", "-c", "gc.auto=0"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn fixture(name: &str) -> Fixture {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sill-wsr-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let source = dir.join("src");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    write(&source.join(".gitignore"), "target/\n");
    for i in 1..=2 {
        write(&source.join(format!("f{i}.txt")), &format!("line {i}\n"));
    }
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "start"]);
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let host = TestHost {
        conn: Mutex::new(conn),
        lives: Mutex::new(HashMap::new()),
        busy: Mutex::new(vec![]),
        pins: Mutex::new(HashMap::new()),
        root: dir.join("workspaces"),
        now: Mutex::new(1_000_000),
    };
    Fixture { dir, host, source }
}

/// A workspace session pinning the source, started.
fn session(f: &Fixture, id: &str) -> api::Workspace {
    f.host.lives.lock().unwrap().insert(id.into(), Life::Alive);
    f.host.pins.lock().unwrap().insert(id.into(), vec![f.source.clone()]);
    f.host.db(|c| set_mode(c, id, true)).unwrap();
    let layout = ensure_for_session(&f.host, id).unwrap();
    assert_eq!(layout.len(), 1);
    session_view(&f.host, id).unwrap().workspaces.remove(0)
}

fn clone_of(w: &api::Workspace) -> PathBuf {
    w.path.to_path().unwrap()
}

fn reason(e: &anyhow::Error) -> String {
    let d = e.downcast_ref::<crate::sfp::DomainError>().unwrap_or_else(|| panic!("not a domain error: {e:#}"));
    d.details.as_ref().and_then(|v| v["reason"].as_str()).unwrap_or(d.code).to_string()
}

fn paths(e: &anyhow::Error) -> Vec<String> {
    let d = e.downcast_ref::<crate::sfp::DomainError>().unwrap();
    serde_json::from_value(d.details.as_ref().unwrap()["paths"].clone()).unwrap()
}

fn apply_req(id: &str, w: &api::Workspace, revision: &api::Revision) -> api::Apply {
    api::Apply { operation_id: id.into(), workspace_id: w.workspace_id.clone(), revision: revision.clone() }
}

fn rev(f: &Fixture, w: &api::Workspace) -> api::Revision {
    get(&f.host, &w.workspace_id).unwrap().revision
}

#[test]
fn a_workspace_session_gets_a_workspace_per_pinned_repository_and_live_ones_none() {
    let f = fixture("ensure");
    // a live session: nothing
    f.host.lives.lock().unwrap().insert("live".into(), Life::Alive);
    f.host.pins.lock().unwrap().insert("live".into(), vec![f.source.clone()]);
    assert!(ensure_for_session(&f.host, "live").unwrap().is_empty());
    // a workspace session pinning the repository, a plain folder and a refused repository
    let plain = f.dir.join("notes");
    std::fs::create_dir_all(&plain).unwrap();
    let sub = f.dir.join("with-submodules");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    write(&sub.join(".gitmodules"), "[submodule \"x\"]\n\tpath = x\n\turl = ./x\n");
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-qm", "sub"]);
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Alive);
    f.host.pins.lock().unwrap().insert("s1".into(), vec![f.source.clone(), plain.clone(), sub.clone()]);
    f.host.db(|c| set_mode(c, "s1", true)).unwrap();
    let layout = ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(layout.len(), 1);
    assert_eq!(layout[0].source, f.source);
    assert_eq!(layout[0].objects, f.source.join(".git/objects"));
    assert_eq!(read(&layout[0].clone.join("f1.txt")), "line 1\n");
    let view = session_view(&f.host, "s1").unwrap();
    assert!(view.workspaces_mode);
    assert_eq!(view.live.len(), 1);
    assert_eq!(view.live[0].reason, "submodules");
    assert!(view.live[0].message.starts_with("Live:"));
    // the next start keeps the same workspace
    let again = ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(again, layout);
    assert_eq!(f.host.db(|c| session_layout(c, "s1")).unwrap(), layout);
    // and the agent is told
    let notice = f.host.db(|c| session_notice(c, "s1")).unwrap().unwrap();
    assert!(notice.contains("Do not push") && notice.contains(&f.source.display().to_string()), "{notice}");
    assert!(f.host.db(|c| session_notice(c, "live")).unwrap().is_none());
}

#[test]
fn apply_repeatedly_during_the_session() {
    let f = fixture("apply");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "committed\n");
    git(&clone, &["commit", "-qam", "agent"]);
    write(&clone.join("f2.txt"), "uncommitted\n");
    write(&clone.join("new.txt"), "new untracked file\n");
    let detail = get(&f.host, &w.workspace_id).unwrap();
    assert_eq!(detail.summary.commits, 1);
    assert_eq!(detail.summary.files, 3);
    assert_eq!(detail.summary.uncommitted, vec!["f2.txt".to_string(), "new.txt".to_string()]);
    let diff = diff(&f.host, &api::DiffRequest { workspace_id: w.workspace_id.clone(), against: api::Against::Applied }).unwrap();
    assert!(diff.text.contains("+committed") && diff.text.contains("+uncommitted") && diff.text.contains("+new untracked file"));
    let done = apply(&f.host, &apply_req("a1", &w, &detail.revision)).unwrap();
    assert_eq!(done.workspace.unwrap().state, api::WorkspaceState::Open, "the workspace goes on");
    assert_eq!(read(&f.source.join("f1.txt")), "committed\n");
    assert_eq!(read(&f.source.join("f2.txt")), "uncommitted\n");
    assert_eq!(git(&f.source, &["log", "-1", "--format=%s"]), "agent");
    // nothing left to bring; then more work, applied again
    let after = get(&f.host, &w.workspace_id).unwrap();
    assert_eq!((after.summary.commits, after.summary.files), (0, 0));
    assert!(!after.summary.uncommitted.is_empty(), "applied files remain dirty in the workspace");
    write(&clone.join("new.txt"), "new untracked file, edited\n");
    assert_eq!(get(&f.host, &w.workspace_id).unwrap().summary.files, 1);
    write(&clone.join("f2.txt"), "uncommitted, again\n");
    apply(&f.host, &apply_req("a2", &w, &rev(&f, &w))).unwrap();
    assert_eq!(read(&f.source.join("f2.txt")), "uncommitted, again\n");
    // the same request again does nothing more
    assert!(apply(&f.host, &apply_req("a2", &w, &rev(&f, &w))).is_ok());
}

#[test]
fn apply_refuses_what_the_person_did_not_see_and_their_own_edits() {
    let f = fixture("refuse");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent\n");
    let seen = rev(&f, &w);
    write(&clone.join("f1.txt"), "agent, later\n");
    assert_eq!(reason(&apply(&f.host, &apply_req("a1", &w, &seen)).unwrap_err()), "stale_revision");
    f.host.busy.lock().unwrap().push("s1".into());
    assert_eq!(reason(&apply(&f.host, &apply_req("a2", &w, &rev(&f, &w))).unwrap_err()), "writer_active");
    f.host.busy.lock().unwrap().clear();
    write(&f.source.join("f1.txt"), "the person's\n");
    let e = apply(&f.host, &apply_req("a3", &w, &rev(&f, &w))).unwrap_err();
    assert_eq!((reason(&e), paths(&e)), ("source_overlap".to_string(), vec!["f1.txt".to_string()]));
    assert_eq!(read(&f.source.join("f1.txt")), "the person's\n");
}

#[test]
fn discard_and_undo_during_the_session() {
    let f = fixture("discard");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "kept\n");
    apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap();
    write(&clone.join("f2.txt"), "unwanted\n");
    write(&clone.join("target/out"), "build\n");
    let gone = discard(&f.host, &apply_req("d1", &w, &rev(&f, &w))).unwrap();
    let d = gone.discarded.unwrap();
    assert_eq!(read(&clone.join("f2.txt")), "line 2\n");
    assert_eq!(read(&clone.join("f1.txt")), "kept\n", "what was applied stays");
    assert!(clone.join("target/out").exists());
    let listed = list(&f.host, &api::List { session_id: Some("s1".into()), after: None, limit: None }).unwrap();
    assert_eq!(listed.discarded.len(), 1);
    let back = undo(&f.host, &api::Undo { operation_id: "u1".into(), discard_id: d.discard_id.clone() }).unwrap();
    assert!(back.workspace.is_some());
    assert_eq!(read(&clone.join("f2.txt")), "unwanted\n");
    assert!(list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap().discarded.is_empty());
    // new work after a Discard: Undo would overwrite it
    let d2 = discard(&f.host, &apply_req("d2", &w, &rev(&f, &w))).unwrap().discarded.unwrap();
    write(&clone.join("f1.txt"), "newer\n");
    assert_eq!(reason(&undo(&f.host, &api::Undo { operation_id: "u2".into(), discard_id: d2.discard_id.clone() }).unwrap_err()), "changed_since");
    // retention runs out
    *f.host.now.lock().unwrap() += RETENTION + 1;
    assert_eq!(reason(&undo(&f.host, &api::Undo { operation_id: "u3".into(), discard_id: d2.discard_id }).unwrap_err()), "retention_expired");
    assert_eq!(sweep(&f.host).unwrap().len(), 1);
    assert_eq!(git(&f.source, &["for-each-ref", "--format=%(refname)", "refs/sill/workspaces/"]).lines().filter(|l| l.contains("discarded")).count(), 0);
}

#[test]
fn an_ended_session_frees_its_folder_as_the_warm_spare() {
    let f = fixture("spare");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("target/debug/app"), "warm\n");
    // the session read only, then ended: its folder becomes the spare, nothing is shown
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    let listed = list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert!(listed.workspaces.is_empty());
    assert!(!clone.exists());
    assert!(flow::spare_path(&f.host.root, &f.source).join("target/debug/app").exists());
    // the next session gets it, warm
    let w2 = session(&f, "s2");
    let clone2 = clone_of(&w2);
    assert_eq!(read(&clone2.join("target/debug/app")), "warm\n");
    assert!(!flow::spare_path(&f.host.root, &f.source).exists());
    // a session that ends with work left: a result, shown; applied, then freed
    write(&clone2.join("f1.txt"), "work\n");
    f.host.lives.lock().unwrap().insert("s2".into(), Life::Ended);
    let listed = list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert_eq!(listed.workspaces[0].state, api::WorkspaceState::Result);
    let done = apply(&f.host, &apply_req("a1", &w2, &rev(&f, &w2))).unwrap();
    assert!(done.workspace.is_none(), "nothing left: freed");
    assert_eq!(read(&f.source.join("f1.txt")), "work\n");
    assert!(flow::spare_path(&f.host.root, &f.source).exists());
}

#[test]
fn undo_after_the_folder_is_gone_makes_a_workspace_of_its_own() {
    let f = fixture("undo-gone");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "discarded work\n");
    let d = discard(&f.host, &apply_req("d1", &w, &rev(&f, &w))).unwrap().discarded.unwrap();
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert!(!clone.exists(), "freed at the session's end");
    let back = undo(&f.host, &api::Undo { operation_id: "u1".into(), discard_id: d.discard_id }).unwrap();
    let w2 = back.workspace.unwrap();
    assert_eq!(w2.state, api::WorkspaceState::Result);
    assert_eq!(read(&clone_of(&w2).join("f1.txt")), "discarded work\n");
}

#[test]
fn prepare_rebases_onto_a_moved_checkout_keeping_uncommitted_work() {
    let f = fixture("rebase");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent\n");
    git(&clone, &["commit", "-qam", "agent"]);
    write(&clone.join("f2.txt"), "agent uncommitted\n");
    write(&f.source.join("new-host.txt"), "host\n");
    git(&f.source, &["add", "."]);
    git(&f.source, &["commit", "-qm", "host"]);
    assert!(get(&f.host, &w.workspace_id).unwrap().needs_rebase);
    assert_eq!(reason(&apply(&f.host, &apply_req("a0", &w, &rev(&f, &w))).unwrap_err()), "stale_revision");
    let p = prepare(&f.host, &api::Operate { operation_id: "p1".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert!(!p.needs_rebase && p.conflicts.is_empty());
    assert_eq!(read(&clone.join("f2.txt")), "agent uncommitted\n", "uncommitted work kept");
    apply(&f.host, &apply_req("a1", &w, &p.revision)).unwrap();
    assert_eq!(read(&f.source.join("f1.txt")), "agent\n");
    assert_eq!(read(&f.source.join("f2.txt")), "agent uncommitted\n");
    assert_eq!(read(&f.source.join("new-host.txt")), "host\n");
    // a conflicting move: the rebase stops, shown; abort puts it back
    write(&clone.join("f1.txt"), "agent again\n");
    git(&clone, &["commit", "-qam", "agent 2"]);
    git(&f.source, &["checkout", "-q", "--", "f2.txt"]);
    write(&f.source.join("f1.txt"), "host again\n");
    git(&f.source, &["commit", "-qam", "host 2"]);
    let p = prepare(&f.host, &api::Operate { operation_id: "p2".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert_eq!(p.conflicts, vec!["f1.txt".to_string()]);
    assert_eq!(p.workspace.operation.as_ref().map(|o| o.state.as_str()), Some("conflict"));
    let aborted = abort(&f.host, &api::Operate { operation_id: "x".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert!(aborted.workspace.unwrap().operation.is_none());
}

#[test]
fn reconcile_removes_a_half_made_workspace() {
    let f = fixture("reconcile");
    let src = super::super::inspect(&f.source).unwrap().unwrap();
    let at = f.host.root.join("src/half-ws-x");
    super::super::create(&src, "ws-x", "half", &at, "op-half").unwrap();
    f.host.db(|c| intent(c, "op-half", "ws-x", "create", &json!({"path": bytes(&at), "source": bytes(&f.source)}), 1)).unwrap();
    reconcile(&f.host).unwrap();
    assert!(!at.exists());
    assert_eq!(git(&f.source, &["for-each-ref", "refs/sill/workspaces/ws-x/"]), "");
}

#[test]
fn a_store_gives_the_jail_its_workspaces_and_keeps_policy_pins() {
    let f = fixture("store");
    let w = session(&f, "s1");
    let db = f.dir.join("store.db");
    let store = crate::store::Store::open(&db).unwrap();
    init(store.conn()).unwrap();
    set_mode(store.conn(), "s1", true).unwrap();
    let r = f.host.db(|c| load(c, &w.workspace_id)).unwrap().unwrap();
    save(store.conn(), &r).unwrap();
    store.pin("s1", &f.source).unwrap();
    // the source stays a pin (its repository's policy fragments apply); the jail binds the clone there
    assert!(store.jail_pins("s1").unwrap().contains(&f.source));
    let binds = store.workspace_mask("s1").unwrap();
    assert_eq!(binds.len(), 1);
    assert_eq!(binds[0].source.to_path().unwrap(), f.source);
    assert_eq!(binds[0].clone.to_path().unwrap(), clone_of(&w));
    assert!(store.workspace_mask("other").unwrap().is_empty());
}

/// The person changes a running workspace session's pins: the next start matches them. A new
/// pin gets its workspace; a removed one leaves the jail — freed to the spare with nothing left,
/// a result on Changes with work waiting — and opens again when pinned again.
#[test]
fn a_start_after_a_pin_change_matches_the_workspaces_to_the_pins() {
    let f = fixture("repin");
    let other = f.dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    write(&other.join("o.txt"), "other\n");
    git(&other, &["add", "."]);
    git(&other, &["commit", "-qm", "other"]);
    let set_pins = |pins: Vec<PathBuf>| {
        f.host.pins.lock().unwrap().insert("s1".into(), pins);
    };
    let mounted = || f.host.db(|c| session_mounts(c, "s1")).unwrap().into_iter().map(|l| l.source).collect::<Vec<_>>();
    let w = session(&f, "s1");
    assert_eq!(mounted(), [f.source.clone()]);

    // nothing done in it: unpinned, it becomes the spare and the new pin gets its own
    set_pins(vec![other.clone()]);
    let layout = ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(layout.iter().map(|l| l.source.clone()).collect::<Vec<_>>(), [other.clone()]);
    assert_eq!(mounted(), [other.clone()]);
    assert!(f.host.db(|c| load(c, &w.workspace_id)).unwrap().is_none(), "freed");
    assert!(spare(&f.host, &f.source).exists(), "kept warm as the spare");

    // pinned again with work left in it, then unpinned: a result, not mounted, still offered
    set_pins(vec![f.source.clone(), other.clone()]);
    ensure_for_session(&f.host, "s1").unwrap();
    let again = session_view(&f.host, "s1").unwrap().workspaces.into_iter().find(|x| x.source.to_path().unwrap() == f.source).unwrap();
    write(&clone_of(&again).join("f1.txt"), "work\n");
    set_pins(vec![other.clone()]);
    ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(mounted(), [other.clone()], "the unpinned workspace is out of the jail");
    let kept = f.host.db(|c| load(c, &again.workspace_id)).unwrap().unwrap();
    assert_eq!(kept.state, "result");
    assert_eq!(read(&kept.path.join("f1.txt")), "work\n", "its work is kept");
    assert!(f.host.db(|c| session_layout(c, "s1")).unwrap().iter().any(|l| l.source == f.source), "the rebase helper still binds it");
    assert!(get(&f.host, &again.workspace_id).unwrap().summary.files == 1, "offered for Apply");

    // pinned once more: the same workspace opens again, its work in it
    set_pins(vec![f.source.clone(), other.clone()]);
    ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(f.host.db(|c| load(c, &again.workspace_id)).unwrap().unwrap().state, "open");
    let mut both = mounted();
    both.sort();
    let mut want = vec![f.source.clone(), other.clone()];
    want.sort();
    assert_eq!(both, want);
}

/// The window's live diff and revert accept sill's own clones, as they accept pinned
/// repositories: a workspace session's drawer reads the clone (royal-horse, 2026-11-09: "… is not
/// a pinned repository" for a file the agent changed).
#[test]
fn every_clone_there_is_known_and_a_freed_one_is_not() {
    let f = fixture("clones");
    assert!(f.host.db(|c| all_clones(c)).unwrap().is_empty());
    let w = session(&f, "s1");
    assert_eq!(f.host.db(|c| all_clones(c)).unwrap(), [clone_of(&w)]);
    f.host.pins.lock().unwrap().insert("s1".into(), vec![]);
    ensure_for_session(&f.host, "s1").unwrap();
    assert!(f.host.db(|c| all_clones(c)).unwrap().is_empty(), "freed to the spare: no longer a clone of anyone's");
}

/// One request tells the window everything about a session's workspaces: the view carries each
/// workspace's detail, the same as `get` gives, from one look; the list does too. A workspace
/// whose repository is gone is listed without a detail.
#[test]
fn the_session_view_and_the_list_carry_each_workspaces_detail() {
    let f = fixture("details");
    let w = session(&f, "s1");
    write(&clone_of(&w).join("f1.txt"), "work\n");
    let view = session_view(&f.host, "s1").unwrap();
    assert_eq!(view.details.len(), 1);
    let d = &view.details[0];
    assert_eq!(d.workspace.workspace_id, w.workspace_id);
    assert_eq!(d.summary.files, 1);
    assert_eq!(d.revision, get(&f.host, &w.workspace_id).unwrap().revision);
    let listed = list(&f.host, &api::List { session_id: Some("s1".into()), after: None, limit: None }).unwrap();
    assert_eq!(listed.details.len(), 1);
    assert_eq!(listed.details[0].summary.files, 1);
    // the repository gone: the record stays, nothing to look at
    std::fs::rename(&f.source, f.dir.join("elsewhere")).unwrap();
    let view = session_view(&f.host, "s1").unwrap();
    assert_eq!(view.workspaces.len(), 1);
    assert!(view.details.is_empty());
    assert!(get(&f.host, &w.workspace_id).is_err());
}

/// A workspace starts from HEAD (§5): the person's own uncommitted work in the checkout is not in
/// it, and that is said — counted in the detail, named to the agent. The agent's applied work,
/// dirty in the checkout too, is not the person's.
#[test]
fn the_persons_uncommitted_work_is_counted_as_not_in_the_workspace() {
    let f = fixture("checkout-only");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    let only = |f: &Fixture| get(&f.host, &w.workspace_id).unwrap().checkout_only;
    let notice = |f: &Fixture| f.host.db(|c| session_notice(c, "s1")).unwrap().unwrap();
    assert_eq!(only(&f), 0);
    assert!(!notice(&f).contains("uncommitted file"), "{}", notice(&f));
    // the person's edit, a staged one and a new file
    write(&f.source.join("f1.txt"), "the person's\n");
    write(&f.source.join("f2.txt"), "the person's, staged\n");
    git(&f.source, &["add", "f2.txt"]);
    write(&f.source.join("mine.txt"), "new\n");
    assert_eq!(only(&f), 3);
    let said = notice(&f);
    assert!(said.contains("3 uncommitted files") && said.contains("f1.txt, f2.txt, mine.txt"), "{said}");
    assert_eq!(get(&f.host, &w.workspace_id).unwrap().summary.files, 0, "nothing of it is the agent's to apply");
    // the same content in the workspace is not missing from it
    write(&clone.join("mine.txt"), "new\n");
    assert_eq!(only(&f), 2);
    // committed or reverted: gone
    git(&f.source, &["checkout", "-q", "HEAD", "--", "f1.txt", "f2.txt"]);
    std::fs::remove_file(f.source.join("mine.txt")).unwrap();
    std::fs::remove_file(clone.join("mine.txt")).unwrap();
    assert_eq!(only(&f), 0);
    // the agent's applied work is dirty in the checkout and not the person's, also once the
    // agent has gone on from it
    write(&clone.join("f1.txt"), "agent\n");
    apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap();
    assert_eq!(read(&f.source.join("f1.txt")), "agent\n");
    assert_eq!(only(&f), 0);
    write(&clone.join("f1.txt"), "agent, later\n");
    assert_eq!(only(&f), 0);
    assert!(!notice(&f).contains("uncommitted file"));
    // the person's edit on top of it is theirs
    write(&f.source.join("f1.txt"), "agent, and the person\n");
    assert_eq!(only(&f), 1);
}
