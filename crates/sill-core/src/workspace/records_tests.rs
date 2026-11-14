//! §12 records against real repositories, with a test host whose sessions' lives, turns and pins
//! the test sets: workspaces made at a session's start, Apply and Discard during it, Undo, a
//! finished folder removed, rebase, and what a restart finds.
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
    overlay: Mutex<Option<Overlay>>,
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
    fn title(&self, session: &str) -> Option<String> {
        Some(format!("Session {session}"))
    }
    fn overlay(&self, _session: &str) -> Result<Option<Overlay>> {
        Ok(self.overlay.lock().unwrap().clone())
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
    // who the person is here: sill's own commit of an ended session's work is theirs
    git(&source, &["config", "user.name", "Pat Person"]);
    git(&source, &["config", "user.email", "pat@example.org"]);
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let host = TestHost {
        conn: Mutex::new(conn),
        lives: Mutex::new(HashMap::new()),
        busy: Mutex::new(vec![]),
        pins: Mutex::new(HashMap::new()),
        root: dir.join("workspaces"),
        now: Mutex::new(1_000_000),
        overlay: Mutex::new(None),
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

/// The agent commits everything in its workspace.
fn commit(clone: &Path, message: &str) {
    git(clone, &["add", "-A"]);
    git(clone, &["commit", "-qm", message]);
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
    let diff = diff(&f.host, &api::DiffRequest { workspace_id: w.workspace_id.clone(), against: api::Against::Applied, path: None }).unwrap();
    assert!(diff.text.contains("+committed") && diff.text.contains("+uncommitted") && diff.text.contains("+new untracked file"));
    // path by path, committed and uncommitted alike, each with its own diff
    let listed: Vec<(std::path::PathBuf, &str, &str, &str)> = detail.files.iter().map(|c| (c.path.to_path().unwrap(), c.status.as_str(), c.added.as_str(), c.removed.as_str())).collect();
    assert_eq!(listed, [(f.source.join("f1.txt"), "M", "1", "1"), (f.source.join("f2.txt"), "M", "1", "1"), (f.source.join("new.txt"), "A", "1", "0")]);
    let one = |path: &std::path::Path| super::diff(&f.host, &api::DiffRequest { workspace_id: w.workspace_id.clone(), against: api::Against::Applied, path: Some(HostPath::from_path(path)) });
    let only = one(&f.source.join("new.txt")).unwrap().text;
    assert!(only.contains("+new untracked file") && !only.contains("committed"), "{only}");
    assert!(one(&f.source.join("../elsewhere")).is_err(), "not a path of the repository");
    assert!(one(std::path::Path::new("/etc/passwd")).is_err());
    assert!(one(&f.source).is_err(), "the repository itself is the whole diff, asked without a path");
    // commits only: while the session's agent can commit, uncommitted work is its to commit
    assert_eq!(reason(&apply(&f.host, &apply_req("a0", &w, &detail.revision)).unwrap_err()), "uncommitted");
    assert_eq!(git(&f.source, &["status", "--porcelain"]), "", "nothing written");
    commit(&clone, "agent, the rest");
    let committed = get(&f.host, &w.workspace_id).unwrap();
    assert_eq!((committed.summary.commits, committed.summary.files), (2, 3));
    assert!(committed.summary.uncommitted.is_empty());
    assert_eq!(committed.state_tree, detail.state_tree, "the same work, now committed");
    assert_eq!(committed.files, detail.files);
    let done = apply(&f.host, &apply_req("a1", &w, &committed.revision)).unwrap();
    assert_eq!(done.workspace.unwrap().state, api::WorkspaceState::Open, "the workspace goes on");
    assert_eq!(read(&f.source.join("f1.txt")), "committed\n");
    assert_eq!(read(&f.source.join("f2.txt")), "uncommitted\n");
    assert_eq!(read(&f.source.join("new.txt")), "new untracked file\n");
    assert_eq!(git(&f.source, &["log", "-2", "--format=%s"]), "agent, the rest\nagent");
    assert_eq!(git(&f.source, &["status", "--porcelain"]), "", "nothing arrives uncommitted");
    // nothing left to bring; then more work, applied again
    let after = get(&f.host, &w.workspace_id).unwrap();
    assert_eq!((after.summary.commits, after.summary.files), (0, 0));
    assert!(after.summary.uncommitted.is_empty());
    write(&clone.join("new.txt"), "new untracked file, edited\n");
    assert_eq!(get(&f.host, &w.workspace_id).unwrap().summary.files, 1);
    write(&clone.join("f2.txt"), "uncommitted, again\n");
    commit(&clone, "a fix after testing");
    let again = apply_req("a2", &w, &rev(&f, &w));
    apply(&f.host, &again).unwrap();
    assert_eq!(read(&f.source.join("f2.txt")), "uncommitted, again\n");
    // the same request again does nothing more
    let tip = git(&f.source, &["rev-parse", "HEAD"]);
    assert!(apply(&f.host, &again).is_ok());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"]), tip);
}

/// A workspace no agent will write again: sill commits what is uncommitted as the person, under
/// the session's title, and the Apply brings it. Nothing left, the workspace is done with.
#[test]
fn an_ended_sessions_uncommitted_work_is_committed_by_sill_and_applied() {
    let f = fixture("ended-apply");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent\n");
    write(&clone.join("new.txt"), "new\n");
    write(&clone.join("target/out"), "build\n");
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    let d = get(&f.host, &w.workspace_id).unwrap();
    assert_eq!(d.workspace.state, api::WorkspaceState::Result);
    assert_eq!(d.summary.uncommitted, vec!["f1.txt".to_string(), "new.txt".to_string()]);
    let done = apply(&f.host, &apply_req("a1", &w, &d.revision)).unwrap();
    assert!(done.workspace.is_none(), "applied, the result is done with");
    assert!(!clone.exists());
    assert_eq!(git(&f.source, &["log", "-1", "--format=%an <%ae>|%cn|%s"]), "Pat Person <pat@example.org>|Pat Person|Session s1");
    assert_eq!((read(&f.source.join("f1.txt")), read(&f.source.join("new.txt"))), ("agent\n".into(), "new\n".into()));
    assert!(!f.source.join("target").exists(), "ignored build output is not committed");
    assert_eq!(git(&f.source, &["status", "--porcelain"]), "");
}

#[test]
fn apply_refuses_what_the_person_did_not_see_and_their_own_edits() {
    let f = fixture("refuse");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent\n");
    commit(&clone, "agent");
    let seen = rev(&f, &w);
    write(&clone.join("f1.txt"), "agent, later\n");
    assert_eq!(reason(&apply(&f.host, &apply_req("a1", &w, &seen)).unwrap_err()), "stale_revision");
    commit(&clone, "agent, later");
    assert_eq!(reason(&apply(&f.host, &apply_req("a1b", &w, &seen)).unwrap_err()), "stale_revision");
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
    commit(&clone, "kept");
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
fn an_ended_session_with_nothing_left_frees_its_folder() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("free");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("target/debug/app"), "built\n");
    // a tool's folder it may not write itself (a module cache)
    write(&clone.join("target/cache/m.txt"), "kept read-only\n");
    std::fs::set_permissions(clone.join("target/cache"), std::fs::Permissions::from_mode(0o555)).unwrap();
    // the session read only, then ended: its folder goes, nothing is shown
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    let listed = list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert!(listed.workspaces.is_empty());
    assert!(!clone.exists());
    // a session that ends with work left: a result, shown; applied, then freed
    let w2 = session(&f, "s2");
    let clone2 = clone_of(&w2);
    write(&clone2.join("f1.txt"), "work\n");
    f.host.lives.lock().unwrap().insert("s2".into(), Life::Ended);
    let listed = list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert_eq!(listed.workspaces[0].state, api::WorkspaceState::Result);
    let done = apply(&f.host, &apply_req("a1", &w2, &rev(&f, &w2))).unwrap();
    assert!(done.workspace.is_none(), "nothing left: freed");
    assert_eq!(read(&f.source.join("f1.txt")), "work\n");
    assert!(!clone2.exists());
}

/// Spare folders an earlier version kept (one finished folder per repository) go at the
/// daemon's start: a new workspace takes its build output from the checkout.
#[test]
fn a_start_removes_the_spare_folders_of_before() {
    let f = fixture("old-spare");
    write(&f.host.root.join(".spare/7fda970fa7eda2a1/target/debug/app"), "old\n");
    reconcile(&f.host).unwrap();
    assert!(!f.host.root.join(".spare").exists());
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
fn prepare_rebases_onto_a_moved_checkout_once_the_work_is_committed() {
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
    // uncommitted work is the agent's to commit before the rebase: nothing is rebased under it
    assert_eq!(reason(&prepare(&f.host, &api::Operate { operation_id: "p0".into(), workspace_id: w.workspace_id.clone() }).unwrap_err()), "uncommitted");
    assert_eq!(git(&clone, &["log", "-1", "--format=%s"]), "agent");
    commit(&clone, "agent, the rest");
    let p = prepare(&f.host, &api::Operate { operation_id: "p1".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert!(!p.needs_rebase && p.conflicts.is_empty());
    assert_eq!(read(&clone.join("f2.txt")), "agent uncommitted\n");
    apply(&f.host, &apply_req("a1", &w, &p.revision)).unwrap();
    assert_eq!(read(&f.source.join("f1.txt")), "agent\n");
    assert_eq!(read(&f.source.join("f2.txt")), "agent uncommitted\n");
    assert_eq!(read(&f.source.join("new-host.txt")), "host\n");
    // a conflicting move: the rebase stops, shown; abort puts it back
    write(&clone.join("f1.txt"), "agent again\n");
    git(&clone, &["commit", "-qam", "agent 2"]);
    write(&f.source.join("f1.txt"), "host again\n");
    git(&f.source, &["commit", "-qam", "host 2"]);
    let p = prepare(&f.host, &api::Operate { operation_id: "p2".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert_eq!(p.conflicts, vec!["f1.txt".to_string()]);
    assert_eq!(p.workspace.operation.as_ref().map(|o| o.state.as_str()), Some("conflict"));
    let aborted = abort(&f.host, &api::Operate { operation_id: "x".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert!(aborted.workspace.unwrap().operation.is_none());
}

/// The agent, asked to Resolve, finishes the stopped rebase in the workspace: the next look
/// shows no stopped rebase, so the shelf offers Apply (the person, 2026-11-09: after Resolve
/// they had to click Abort rebase before Apply was there again).
#[test]
fn a_rebase_finished_in_the_workspace_is_no_longer_shown_as_stopped() {
    let f = fixture("resolved");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent\n");
    git(&clone, &["commit", "-qam", "agent"]);
    write(&f.source.join("f1.txt"), "host\n");
    git(&f.source, &["commit", "-qam", "host"]);
    let op = |id: &str| api::Operate { operation_id: id.into(), workspace_id: w.workspace_id.clone() };
    let p = prepare(&f.host, &op("p1")).unwrap();
    assert_eq!(p.conflicts, vec!["f1.txt".to_string()]);
    let shown = || session_view(&f.host, "s1").unwrap().workspaces.remove(0).operation.map(|o| o.state);
    assert_eq!(shown().as_deref(), Some("conflict"), "stopped: Resolve and Abort rebase");
    // resolved and continued in the workspace, as the agent does
    write(&clone.join("f1.txt"), "both\n");
    git(&clone, &["add", "f1.txt"]);
    let done = Command::new("git").arg("-C").arg(&clone).args(["-c", "user.name=t", "-c", "user.email=t@t", "rebase", "--continue"]).env("GIT_EDITOR", "true").output().unwrap();
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert_eq!(shown(), None, "finished: Apply");
    let d = get(&f.host, &w.workspace_id).unwrap();
    assert!(d.workspace.operation.is_none() && !d.needs_rebase);
    apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap();
    assert_eq!(read(&f.source.join("f1.txt")), "both\n");
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
    // the source stays a pin; the jail binds the clone there
    assert!(store.jail_pins("s1").unwrap().contains(&f.source));
    let binds = store.workspace_mask("s1").unwrap();
    assert_eq!(binds.len(), 1);
    assert_eq!(binds[0].source.to_path().unwrap(), f.source);
    assert_eq!(binds[0].clone.to_path().unwrap(), clone_of(&w));
    assert!(store.workspace_mask("other").unwrap().is_empty());
}

/// The person changes a running workspace session's pins: the next start matches them. A new
/// pin gets its workspace; a removed one leaves the jail — freed with nothing left,
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

    // nothing done in it: unpinned, it goes and the new pin gets its own
    set_pins(vec![other.clone()]);
    let layout = ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(layout.iter().map(|l| l.source.clone()).collect::<Vec<_>>(), [other.clone()]);
    assert_eq!(mounted(), [other.clone()]);
    assert!(f.host.db(|c| load(c, &w.workspace_id)).unwrap().is_none(), "freed");
    assert!(!clone_of(&w).exists(), "its folder with it");

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
    assert!(f.host.db(|c| all_clones(c)).unwrap().is_empty(), "freed: no longer a clone of anyone's");
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
    // the agent's applied work is the person's commit, not an edit of theirs, also once the
    // agent has gone on from it
    write(&clone.join("f1.txt"), "agent\n");
    commit(&clone, "agent");
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

// --- work from before the pin (§12, carry) ---

/// Now, in unix seconds.
fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The session's overlay as a jail over `f.dir` leaves it: its upper tree, the repository's place
/// in it returned. The agent's files get written there with `agent_wrote`.
fn overlay(f: &Fixture, started: i64) -> PathBuf {
    let upper = f.dir.join("upper");
    std::fs::create_dir_all(&upper).unwrap();
    *f.host.overlay.lock().unwrap() = Some(Overlay { layers: vec![(f.dir.clone(), upper.clone())], started });
    upper.join("src")
}

/// A file the agent wrote at `when` (unix seconds) into the overlay's copy of the repository.
fn agent_wrote(upper_repo: &Path, rel: &str, text: &str, when: i64) {
    let p = upper_repo.join(rel);
    write(&p, text);
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(when as u64);
    std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
}

/// A commit in the person's checkout whose reflog entry is dated `when`.
fn commit_at(dir: &Path, when: i64, message: &str) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_COMMITTER_DATE", format!("@{when} +0000"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn operation(f: &Fixture, w: &api::Workspace) -> Option<api::WorkspaceOperation> {
    get(&f.host, &w.workspace_id).unwrap().workspace.operation
}

#[test]
fn work_from_before_the_pin_is_carried_into_a_new_workspace_uncommitted() {
    let f = fixture("carry");
    let start = git(&f.source, &["rev-parse", "HEAD"]);
    let up = overlay(&f, 0);
    let t = unix_now() + 100;
    agent_wrote(&up, "f1.txt", "agent\n", t);
    agent_wrote(&up, "sub/new.txt", "new\n", t);
    // not carried: the repository's own state, and what it ignores
    agent_wrote(&up, ".git/index", "the overlay's index", t);
    agent_wrote(&up, "target/out", "build", t);
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    assert_eq!(read(&clone.join("f1.txt")), "agent\n");
    assert_eq!(read(&clone.join("sub/new.txt")), "new\n");
    // uncommitted, on the base
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), start);
    assert_eq!(git(&clone, &["status", "--porcelain"]), "M f1.txt\n?? sub/");
    // carried entries left the overlay; the rest stays where it was
    assert!(!up.join("f1.txt").exists() && !up.join("sub").exists());
    assert_eq!(read(&up.join(".git/index")), "the overlay's index");
    assert!(up.join("target/out").exists());
    // nothing for the person to see to: the shelf offers the work as any other
    assert!(operation(&f, &w).is_none());
    // the next start finds nothing more
    ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(read(&clone.join("f1.txt")), "agent\n");
}

/// A build the agent ran before the pin leaves thousands of ignored files in the overlay: more
/// path bytes than a pipe holds, in and out of `check-ignore`. The carry must not wait on it.
#[test]
fn work_from_before_the_pin_is_carried_past_a_builds_worth_of_ignored_files() {
    let f = fixture("carry-build");
    let up = overlay(&f, 0);
    let t = unix_now() + 100;
    agent_wrote(&up, "f1.txt", "agent\n", t);
    let deep = "debug/incremental/a-rather-long-directory-name-as-cargo-makes-them-0123456789abcdef";
    std::fs::create_dir_all(up.join("target").join(deep)).unwrap();
    for i in 0..3000 {
        std::fs::write(up.join("target").join(deep).join(format!("object-file-{i:05}.rcgu.o")), "o").unwrap();
    }
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let w = session(&f, "s1");
            let _ = done.send(w);
        });
        let w = wait.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| {
            // abort: the scope would otherwise wait on the stuck git forever
            std::process::abort()
        });
        assert_eq!(read(&clone_of(&w).join("f1.txt")), "agent\n");
        assert!(!up.join("f1.txt").exists());
        assert!(up.join("target").join(deep).join("object-file-00000.rcgu.o").exists());
    });
}

#[test]
fn work_from_before_the_pin_conflicting_with_a_moved_checkout_stops_a_rebase_for_resolve() {
    let f = fixture("carry-conflict");
    let up = overlay(&f, 0);
    let t = unix_now() + 100;
    // the agent edits f1 from the checkout as it was …
    agent_wrote(&up, "f1.txt", "agent\n", t);
    // … and the person commits their own f1 after
    write(&f.source.join("f1.txt"), "person\n");
    commit_at(&f.source, t + 100, "person");
    let theirs = git(&f.source, &["rev-parse", "HEAD"]);
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    let op = operation(&f, &w).expect("a stopped carry");
    assert_eq!((op.kind.as_str(), op.state.as_str()), ("carry", "conflict"));
    assert!(op.detail.contains("stopped: f1.txt"), "{}", op.detail);
    assert!(clone.join(".git/rebase-merge").exists());
    assert!(read(&clone.join("f1.txt")).contains("<<<<<<<"));
    assert!(!up.join("f1.txt").exists(), "kept by the clone's rebase now");
    // the agent resolves as Resolve asks
    write(&clone.join("f1.txt"), "both\n");
    git(&clone, &["add", "f1.txt"]);
    let out = Command::new("git").arg("-C").arg(&clone).args(["-c", "user.name=t", "-c", "user.email=t@t", "rebase", "--continue"]).env("GIT_EDITOR", "true").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(git(&clone, &["rev-parse", "HEAD^"]), theirs);
    assert!(operation(&f, &w).is_none(), "settled once the rebase is finished");
}

#[test]
fn work_from_before_the_pin_without_a_base_is_taken_as_it_is_and_said() {
    let f = fixture("carry-nobase");
    let up = overlay(&f, 0);
    // written before anything in the checkout's reflog
    agent_wrote(&up, "f2.txt", "agent\n", 1000);
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    assert_eq!(read(&clone.join("f2.txt")), "agent\n");
    let op = operation(&f, &w).expect("a note");
    assert_eq!((op.kind.as_str(), op.state.as_str()), ("carry", "notice"));
    assert!(op.detail.contains("taken as they are"), "{}", op.detail);
    // seen through with the Apply, once the agent has committed the work
    commit(&clone, "the work from before the pin");
    apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap();
    assert!(operation(&f, &w).is_none());
}

#[test]
fn work_from_before_the_pin_conflicting_with_the_workspaces_own_waits_in_the_overlay() {
    let f = fixture("carry-wait");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    // the workspace's own work, not applied
    write(&clone.join("f1.txt"), "own\n");
    // the session's overlay holds other work on the same file (the repository was unpinned a while)
    let up = overlay(&f, 0);
    agent_wrote(&up, "f1.txt", "agent\n", unix_now() + 100);
    ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(read(&clone.join("f1.txt")), "own\n");
    assert_eq!(read(&up.join("f1.txt")), "agent\n", "left in the overlay");
    let op = operation(&f, &w).expect("waiting");
    assert_eq!((op.kind.as_str(), op.state.as_str()), ("carry", "waiting"));
    // once the workspace's work is discarded, the next start brings it
    discard(&f.host, &apply_req("d1", &w, &rev(&f, &w))).unwrap();
    ensure_for_session(&f.host, "s1").unwrap();
    assert_eq!(read(&clone.join("f1.txt")), "agent\n");
    assert!(!up.join("f1.txt").exists());
    assert!(operation(&f, &w).is_none());
}

#[test]
fn undo_does_not_replace_a_reopened_workspace() {
    let f = fixture("undo-reopened");
    let old = session(&f, "s1");
    write(&clone_of(&old).join("f1.txt"), "old work\n");
    let d = discard(&f.host, &apply_req("d", &old, &rev(&f, &old)))
        .unwrap()
        .discarded
        .unwrap();
    f.host
        .lives
        .lock()
        .unwrap()
        .insert("s1".into(), Life::Ended);
    list(
        &f.host,
        &api::List {
            session_id: None,
            after: None,
            limit: None,
        },
    )
    .unwrap();
    let current = session(&f, "s1");
    write(&clone_of(&current).join("f1.txt"), "new work\n");
    let req = api::Undo {
        operation_id: "u".into(),
        discard_id: d.discard_id,
    };
    f.host.busy.lock().unwrap().push("s1".into());
    assert_eq!(reason(&undo(&f.host, &req).unwrap_err()), "writer_active");
    f.host.busy.lock().unwrap().clear();
    assert_eq!(reason(&undo(&f.host, &req).unwrap_err()), "changed_since");
    assert_eq!(
        session_view(&f.host, "s1").unwrap().workspaces[0].workspace_id,
        current.workspace_id
    );
    assert_eq!(read(&clone_of(&current).join("f1.txt")), "new work\n");
}

#[test]
fn discard_database_failure_does_not_reset_work() {
    let f = fixture("discard-db-failure");
    let w = session(&f, "s1");
    write(&clone_of(&w).join("f1.txt"), "precious\n");
    f.host.db(|c| { c.execute_batch("CREATE TRIGGER fail_discard BEFORE INSERT ON workspace_discards BEGIN SELECT RAISE(ABORT, 'injected'); END;")?; Ok(()) }).unwrap();
    assert!(discard(&f.host, &apply_req("d", &w, &rev(&f, &w))).is_err());
    assert_eq!(read(&clone_of(&w).join("f1.txt")), "precious\n");
    reconcile(&f.host).unwrap();
    assert_eq!(read(&clone_of(&w).join("f1.txt")), "precious\n");
}

#[test]
fn unreadable_work_stops_apply_and_discard() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("unreadable-work");
    let w = session(&f, "s1");
    let file = clone_of(&w).join("f1.txt");
    write(&file, "precious\n");
    let seen = rev(&f, &w);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0)).unwrap();
    assert!(apply(&f.host, &apply_req("a", &w, &seen)).is_err());
    assert!(discard(&f.host, &apply_req("d", &w, &seen)).is_err());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(read(&file), "precious\n");
    assert_eq!(read(&f.source.join("f1.txt")), "line 1\n");
}

#[test]
fn discard_interrupted_after_reset_keeps_undo_across_recovery() {
    let f = fixture("discard-after-reset");
    let w = session(&f, "s1");
    write(&clone_of(&w).join("f1.txt"), "precious\n");
    f.host.db(|c| { c.execute_batch("CREATE TRIGGER fail_settle BEFORE UPDATE ON workspace_journal WHEN NEW.kind='discard' AND NEW.state='done' BEGIN SELECT RAISE(ABORT, 'injected'); END;")?; Ok(()) }).unwrap();
    assert!(discard(&f.host, &apply_req("d", &w, &rev(&f, &w))).is_err());
    assert_eq!(read(&clone_of(&w).join("f1.txt")), "line 1\n");
    reconcile(&f.host).unwrap();
    let listed = list(
        &f.host,
        &api::List {
            session_id: None,
            after: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(listed.discarded.len(), 1);
    let first = listed.discarded[0].discard_id.clone();
    f.host
        .db(|c| {
            c.execute_batch("DROP TRIGGER fail_settle;")?;
            Ok(())
        })
        .unwrap();
    write(&clone_of(&w).join("f1.txt"), "second\n");
    discard(&f.host, &apply_req("d2", &w, &rev(&f, &w))).unwrap();
    undo(
        &f.host,
        &api::Undo {
            operation_id: "u".into(),
            discard_id: first,
        },
    )
    .unwrap();
    assert_eq!(read(&clone_of(&w).join("f1.txt")), "precious\n");
}

fn readable(clone: &Path, oid: &str) -> bool {
    Command::new("git").arg("-C").arg(clone).args(["cat-file", "-e", oid]).env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap().status.success()
}

/// A look reads the workspace's files, and the agent chooses what is at their paths: a directory
/// replaced by a link out of the workspace must not make the look read what the link reaches —
/// a host file the jail hides — into the workspace's store, where the agent could read it.
#[test]
fn a_link_planted_in_the_workspace_reads_nothing_outside_it() {
    let f = fixture("link-out");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    let hidden = f.dir.join("hidden");
    write(&hidden.join("key"), "TOP SECRET KEY\n");
    let oid = git(&f.source, &["hash-object", hidden.join("key").to_str().unwrap()]);
    // a tracked path d/key, then d replaced by a link; and an untracked link to the file itself
    write(&clone.join("d/key"), "x\n");
    git(&clone, &["add", "d/key"]);
    std::fs::remove_dir_all(clone.join("d")).unwrap();
    std::os::unix::fs::symlink(&hidden, clone.join("d")).unwrap();
    std::os::unix::fs::symlink(hidden.join("key"), clone.join("direct")).unwrap();
    let d = get(&f.host, &w.workspace_id).unwrap();
    assert!(!readable(&clone, &oid), "the file behind the link is not in the workspace's store");
    let text = diff(&f.host, &api::DiffRequest { workspace_id: w.workspace_id.clone(), against: api::Against::Applied, path: None }).unwrap().text;
    assert!(!text.contains("TOP SECRET"), "{text}");
    // the links themselves are the work: committed and applied as links
    let _ = d;
    commit(&clone, "links");
    assert!(!readable(&clone, &oid), "committing the link does not read through it either");
    apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap();
    assert!(std::fs::symlink_metadata(f.source.join("d")).unwrap().file_type().is_symlink());
    assert!(!readable(&f.source, &oid) && !readable(&clone, &oid));
}

/// Files are hashed from their descriptors, still as git would store them at their paths: an
/// attribute's end-of-line conversion applies, and the executable bit is kept.
#[test]
fn files_read_by_descriptor_are_stored_as_git_stores_them() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("attrs");
    write(&f.source.join(".gitattributes"), "*.bat text eol=crlf\n");
    git(&f.source, &["add", "."]);
    git(&f.source, &["commit", "-qm", "attributes"]);
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("run.bat"), "echo one\r\necho two\r\n");
    write(&clone.join("plain.txt"), "kept\r\nas it is\r\n");
    write(&clone.join("tool.sh"), "#!/bin/sh\n");
    std::fs::set_permissions(clone.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let tree = flow::state_tree(&clone).unwrap();
    let staged = |path: &str| git(&clone, &["hash-object", path]);
    for path in ["run.bat", "plain.txt", "tool.sh"] {
        assert_eq!(git(&clone, &["rev-parse", &format!("{tree}:{path}")]), staged(path), "{path}");
    }
    assert_eq!(git(&clone, &["cat-file", "-p", &format!("{tree}:run.bat")]), "echo one\necho two", "stored with LF");
    assert!(git(&clone, &["ls-tree", &tree, "tool.sh"]).starts_with("100755"));
}

/// An ignored file of the person's is not in their `status`; the workspace un-ignoring its path,
/// or staging it by force, must not make Apply write over it.
#[test]
fn apply_stops_at_an_ignored_file_of_the_persons() {
    for forced in [false, true] {
        let f = fixture("ignored");
        write(&f.source.join(".gitignore"), "target/\nlocal.env\n");
        git(&f.source, &["commit", "-qam", "ignore local.env"]);
        write(&f.source.join("local.env"), "PERSON=only copy\n");
        let w = session(&f, "s1");
        let clone = clone_of(&w);
        write(&clone.join("local.env"), "AGENT=1\n");
        write(&clone.join("f1.txt"), "agent\n");
        if forced {
            git(&clone, &["add", "-f", "local.env"]);
        } else {
            write(&clone.join(".gitignore"), "target/\n");
        }
        commit(&clone, "agent");
        let e = apply(&f.host, &apply_req("a1", &w, &rev(&f, &w))).unwrap_err();
        assert_eq!((reason(&e), paths(&e)), ("untracked_collision".to_string(), vec!["local.env".to_string()]), "forced={forced}");
        assert_eq!(read(&f.source.join("local.env")), "PERSON=only copy\n");
        assert_eq!(read(&f.source.join("f1.txt")), "line 1\n", "nothing written");
        // the same content there already is no collision
        write(&f.source.join("local.env"), "AGENT=1\n");
        apply(&f.host, &apply_req("a2", &w, &rev(&f, &w))).unwrap();
        assert_eq!(read(&f.source.join("f1.txt")), "agent\n");
    }
}

/// An ended session's uncommitted work that conflicts with a commit of the person's: sill
/// commits it as theirs before the rebase, so the rebase stops on it like on any commit — no
/// conflict markers reach the checkout — Abort leaves it committed, and once the rebase is
/// finished in the workspace the Apply brings the commit.
#[test]
fn an_ended_sessions_uncommitted_work_in_conflict_stops_the_rebase() {
    let f = fixture("wip-conflict");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent uncommitted\n");
    write(&clone.join("new.txt"), "untracked\n");
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    write(&f.source.join("f1.txt"), "host\n");
    git(&f.source, &["commit", "-qam", "host"]);
    let prepare_now = |op: &str| prepare(&f.host, &api::Operate { operation_id: op.into(), workspace_id: w.workspace_id.clone() }).unwrap();
    let p = prepare_now("p1");
    assert_eq!(p.conflicts, vec!["f1.txt".to_string()]);
    assert_eq!(p.workspace.operation.as_ref().map(|o| o.state.as_str()), Some("conflict"));
    assert_eq!(reason(&apply(&f.host, &apply_req("a0", &w, &p.revision)).unwrap_err()), "rebase_in_progress");
    assert_eq!(read(&f.source.join("f1.txt")), "host\n");
    // Abort: the work is as it was, in sill's commit of it
    abort(&f.host, &api::Operate { operation_id: "x".into(), workspace_id: w.workspace_id.clone() }).unwrap();
    assert_eq!(read(&clone.join("f1.txt")), "agent uncommitted\n");
    assert_eq!(git(&clone, &["log", "-1", "--format=%an|%s"]), "Pat Person|Session s1");
    assert_eq!(git(&clone, &["status", "--porcelain"]), "");
    // again, resolved in the workspace
    prepare_now("p2");
    write(&clone.join("f1.txt"), "host and agent\n");
    git(&clone, &["add", "f1.txt"]);
    git(&clone, &["-c", "core.editor=true", "rebase", "--continue"]);
    let d = get(&f.host, &w.workspace_id).unwrap();
    assert!(d.workspace.operation.is_none() && !d.needs_rebase);
    assert_eq!(d.summary.commits, 1);
    apply(&f.host, &apply_req("a1", &w, &d.revision)).unwrap();
    assert_eq!(read(&f.source.join("f1.txt")), "host and agent\n");
    assert_eq!(read(&f.source.join("new.txt")), "untracked\n");
    assert_eq!(git(&f.source, &["log", "-2", "--format=%s"]), "Session s1\nhost");
    assert_eq!(git(&f.source, &["status", "--porcelain"]), "");
}

/// Work put back by Undo into a workspace of its own lives in the checkout's store: the
/// checkout's gc must not take it from under the workspace.
#[test]
fn undone_work_in_a_new_workspace_survives_the_checkouts_gc() {
    let f = fixture("undo-gc");
    let w = session(&f, "s1");
    let clone = clone_of(&w);
    write(&clone.join("f1.txt"), "agent commit\n");
    git(&clone, &["commit", "-qam", "agent"]);
    let d = discard(&f.host, &apply_req("d1", &w, &rev(&f, &w))).unwrap().discarded.unwrap();
    f.host.lives.lock().unwrap().insert("s1".into(), Life::Ended);
    list(&f.host, &api::List { session_id: None, after: None, limit: None }).unwrap();
    assert!(!clone.exists());
    let w2 = undo(&f.host, &api::Undo { operation_id: "u1".into(), discard_id: d.discard_id }).unwrap().workspace.unwrap();
    git(&f.source, &["gc", "-q", "--prune=now"]);
    assert_eq!(git(&clone_of(&w2), &["log", "-1", "--format=%s"]), "agent");
    // applied, the workspace is done with and its anchors go
    apply(&f.host, &apply_req("a1", &w2, &rev(&f, &w2))).unwrap();
    assert_eq!(git(&f.source, &["for-each-ref", "--format=%(refname)", "refs/sill/"]), "");
}

#[test]
fn successful_apply_or_discard_clears_interrupted_apply() {
    for discard_work in [false, true] {
        let f = fixture(if discard_work { "interrupted-discard" } else { "interrupted-apply" });
        let w = session(&f, "s1");
        let clone = clone_of(&w);
        write(&clone.join("f1.txt"), "agent work\n");
        commit(&clone, "agent work");
        // Prevent importing the commit: a real Git failure after the journal intent.
        let lock = f.source.join(".git").join(format!("{}.lock", crate::workspace::anchor_ref(&w.workspace_id, "incoming")));
        write(&lock, "locked\n");
        assert!(apply(&f.host, &apply_req("broken", &w, &rev(&f, &w))).is_err());
        let shown = f.host.db(|c| shown_operation(c, &w.workspace_id)).unwrap().unwrap();
        assert_eq!(shown.state, "interrupted");
        std::fs::remove_file(lock).unwrap();
        let request = apply_req("recovered", &w, &rev(&f, &w));
        let done = if discard_work { discard(&f.host, &request) } else { apply(&f.host, &request) }.unwrap();
        assert!(done.workspace.unwrap().operation.is_none(), "successful recovery clears the warning");
        assert!(f.host.db(|c| shown_operation(c, &w.workspace_id)).unwrap().is_none());
        assert_eq!(read(&f.source.join("f1.txt")), if discard_work { "line 1\n" } else { "agent work\n" });
    }
}
