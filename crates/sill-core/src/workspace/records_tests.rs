//! Records, receipts, the journal and every operation against real repositories, with a test
//! host whose sessions' lives the test sets.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use rusqlite::Connection;
use sill_proto::sessions::HostPath;
use sill_proto::workspaces as api;

use super::*;

struct TestHost {
    conn: Mutex<Connection>,
    lives: Mutex<HashMap<String, Life>>,
    created: Mutex<HashMap<String, String>>,
    root: PathBuf,
    now: Mutex<i64>,
}

impl Host for TestHost {
    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.conn.lock().unwrap())
    }
    fn new_session(&self, create_id: &str, _agent: &str, _policy: &str, pin: &Path) -> Result<String> {
        assert!(pin.exists(), "the session is pinned to the made clone");
        let mut created = self.created.lock().unwrap();
        let n = created.len();
        let id = created.entry(create_id.into()).or_insert_with(|| format!("s{n}")).clone();
        self.lives.lock().unwrap().entry(id.clone()).or_insert(Life::Stopped);
        Ok(id)
    }
    fn life(&self, session: &str) -> Life {
        *self.lives.lock().unwrap().get(session).unwrap_or(&Life::Missing)
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
    for i in 1..=2 {
        write(&source.join(format!("f{i}.txt")), &format!("line {i}\n"));
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", &format!("c{i}")]);
    }
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let host = TestHost {
        conn: Mutex::new(conn),
        lives: Mutex::new(HashMap::new()),
        created: Mutex::new(HashMap::new()),
        root: dir.join("workspaces"),
        now: Mutex::new(1_000_000),
    };
    Fixture { dir, host, source }
}

fn create_req(f: &Fixture, op: &str) -> api::Create {
    api::Create { operation_id: op.into(), source: HostPath::from_path(&f.source), name: "Try it".into(), agent: "claude".into(), policy: "default".into() }
}

fn reason(e: &anyhow::Error) -> String {
    let d = e.downcast_ref::<crate::sfp::DomainError>().unwrap_or_else(|| panic!("not a domain error: {e:#}"));
    d.details.as_ref().and_then(|v| v["reason"].as_str()).unwrap_or(d.code).to_string()
}

fn paths(e: &anyhow::Error) -> Vec<String> {
    let d = e.downcast_ref::<crate::sfp::DomainError>().unwrap();
    serde_json::from_value(d.details.as_ref().unwrap()["paths"].clone()).unwrap()
}

/// A made workspace whose session committed work and ended.
fn worked(f: &Fixture) -> api::Workspace {
    let made = create(&f.host, &create_req(f, "op-create")).unwrap();
    let path = made.workspace.path.to_path().unwrap();
    write(&path.join("f2.txt"), "agent work\n");
    git(&path, &["commit", "-qam", "agent"]);
    f.host.lives.lock().unwrap().insert(made.session_id, Life::Ended);
    made.workspace
}

fn op(id: &str, ws: &api::Workspace) -> api::Operate {
    api::Operate { operation_id: id.into(), workspace_id: ws.workspace_id.clone() }
}

fn apply(id: &str, ws: &api::Workspace, revision: &api::Revision) -> api::Apply {
    api::Apply { operation_id: id.into(), workspace_id: ws.workspace_id.clone(), revision: revision.clone() }
}

#[test]
fn create_is_one_workspace_per_operation() {
    let f = fixture("create");
    let made = create(&f.host, &create_req(&f, "op-1")).unwrap();
    let w = &made.workspace;
    assert_eq!(w.state, api::WorkspaceState::Open);
    assert_eq!(w.owner_session.as_deref(), Some(made.session_id.as_str()));
    assert!(w.workspace_id.starts_with("ws-"));
    assert!(w.path.to_path().unwrap().join(".git").is_dir());
    assert!(!made.source_dirty);
    // the same request again: the same workspace, nothing new on disk
    let again = create(&f.host, &create_req(&f, "op-1")).unwrap();
    assert_eq!(again.workspace.workspace_id, w.workspace_id);
    assert_eq!(std::fs::read_dir(f.host.root.join("src")).unwrap().count(), 1);
    // the same id for something else
    let mut other = create_req(&f, "op-1");
    other.name = "Something else".into();
    assert_eq!(reason(&create(&f.host, &other).unwrap_err()), "id_conflict");
    // a source that cannot have one
    git(&f.source, &["checkout", "-q", "--detach"]);
    let e = create(&f.host, &create_req(&f, "op-2")).unwrap_err();
    assert_eq!(reason(&e), "detached_head");
    git(&f.source, &["checkout", "-q", "main"]);
    // a dirty source is reported
    write(&f.source.join("f1.txt"), "mine\n");
    assert!(create(&f.host, &create_req(&f, "op-3")).unwrap().source_dirty);
}

#[test]
fn an_ended_owner_leaves_a_result_and_a_running_one_blocks() {
    let f = fixture("owner");
    let made = create(&f.host, &create_req(&f, "op-1")).unwrap();
    let id = made.workspace.workspace_id.clone();
    f.host.lives.lock().unwrap().insert(made.session_id.clone(), Life::Running);
    assert_eq!(reason(&finish(&f.host, &id).unwrap_err()), "writer_active");
    assert_eq!(get(&f.host, &id).unwrap().workspace.state, api::WorkspaceState::Open);
    // stopped, not ended: still open until the person finishes it
    f.host.lives.lock().unwrap().insert(made.session_id.clone(), Life::Stopped);
    assert_eq!(get(&f.host, &id).unwrap().workspace.state, api::WorkspaceState::Open);
    assert_eq!(finish(&f.host, &id).unwrap().workspace.state, api::WorkspaceState::Result);
    // reopened: the same, still living session
    let reopened = reopen(&f.host, &api::Reopen { operation_id: "op-r".into(), workspace_id: id.clone(), agent: None, policy: None }).unwrap();
    assert_eq!(reopened.session_id.as_deref(), Some(made.session_id.as_str()));
    assert_eq!(reopened.workspace.state, api::WorkspaceState::Open);
    // the session ends: listing shows a result without an owner
    f.host.lives.lock().unwrap().insert(made.session_id.clone(), Life::Ended);
    let listed = list(&f.host, &api::List { after: None, limit: None }).unwrap();
    assert_eq!(listed.workspaces[0].state, api::WorkspaceState::Result);
    assert_eq!(listed.workspaces[0].owner_session, None);
    // reopening now needs a new session
    let e = reopen(&f.host, &api::Reopen { operation_id: "op-r2".into(), workspace_id: id.clone(), agent: None, policy: None }).unwrap_err();
    assert!(format!("{e:#}").contains("agent and policy"));
    let fresh = reopen(&f.host, &api::Reopen { operation_id: "op-r3".into(), workspace_id: id, agent: Some("claude".into()), policy: Some("default".into()) }).unwrap();
    assert_ne!(fresh.session_id.as_deref(), Some(made.session_id.as_str()));
}

#[test]
fn prepare_then_merge_and_a_retry_does_nothing_more() {
    let f = fixture("merge");
    let ws = worked(&f);
    let detail = get(&f.host, &ws.workspace_id).unwrap();
    assert_eq!(detail.summary.as_ref().unwrap().commits, 1);
    assert!(!detail.needs_rebase);
    let diff = diff(&f.host, &api::DiffRequest { workspace_id: ws.workspace_id.clone(), against: api::Against::Base }).unwrap();
    assert!(diff.text.contains("+agent work") && !diff.truncated);
    let prepared = prepare(&f.host, &op("op-p", &ws)).unwrap();
    assert!(prepared.changed && !prepared.needs_rebase);
    assert_eq!(prepared.revision.destination_branch, "main");
    assert!(!prepare(&f.host, &op("op-p2", &ws)).unwrap().changed, "nothing changed since");
    let merged = merge(&f.host, &apply("op-m", &ws, &prepared.revision)).unwrap();
    assert_eq!(merged.workspace.state, api::WorkspaceState::Merged);
    assert_eq!(git(&f.source, &["rev-parse", "main"]), prepared.revision.tip);
    assert_eq!(std::fs::read_to_string(f.source.join("f2.txt")).unwrap(), "agent work\n");
    let again = merge(&f.host, &apply("op-m", &ws, &prepared.revision)).unwrap();
    assert_eq!(again.workspace.state, api::WorkspaceState::Merged);
    assert_eq!(again.operation.unwrap().state, sill_proto::host::OperationState::Completed);
}

#[test]
fn merge_refuses_what_the_person_did_not_see() {
    let f = fixture("refuse");
    let ws = worked(&f);
    let path = ws.path.to_path().unwrap();
    let prepared = prepare(&f.host, &op("op-p", &ws)).unwrap();
    // more agent work after the person looked
    write(&path.join("f1.txt"), "later\n");
    git(&path, &["commit", "-qam", "later"]);
    assert_eq!(reason(&merge(&f.host, &apply("op-m1", &ws, &prepared.revision)).unwrap_err()), "stale_revision");
    // uncommitted work in the workspace
    let prepared = prepare(&f.host, &op("op-p2", &ws)).unwrap();
    write(&path.join("loose.txt"), "x\n");
    let current = get(&f.host, &ws.workspace_id).unwrap().revision.unwrap();
    let e = merge(&f.host, &apply("op-m2", &ws, &current)).unwrap_err();
    assert_eq!(reason(&e), "uncommitted");
    assert_eq!(paths(&e), vec!["loose.txt".to_string()]);
    std::fs::remove_file(path.join("loose.txt")).unwrap();
    // the person's own edit in the way
    write(&f.source.join("f2.txt"), "the person's f2\n");
    let e = merge(&f.host, &apply("op-m3", &ws, &prepared.revision)).unwrap_err();
    assert_eq!(reason(&e), "source_overlap");
    assert_eq!(paths(&e), vec!["f2.txt".to_string()]);
    assert_eq!(std::fs::read_to_string(f.source.join("f2.txt")).unwrap(), "the person's f2\n");
    assert_eq!(get(&f.host, &ws.workspace_id).unwrap().workspace.state, api::WorkspaceState::Result);
    git(&f.source, &["checkout", "-q", "--", "f2.txt"]);
    // the source moved: the revision names the old tip, and a rebase is needed
    write(&f.source.join("f1.txt"), "host\n");
    git(&f.source, &["commit", "-qam", "host"]);
    assert_eq!(reason(&merge(&f.host, &apply("op-m4", &ws, &prepared.revision)).unwrap_err()), "stale_revision");
    let again = prepare(&f.host, &op("op-p3", &ws)).unwrap();
    assert!(again.needs_rebase && again.changed);
    assert_eq!(reason(&merge(&f.host, &apply("op-m5", &ws, &again.revision)).unwrap_err()), "stale_revision");
}

#[test]
fn discard_undo_and_expiry() {
    let f = fixture("discard");
    let ws = worked(&f);
    let path = ws.path.to_path().unwrap();
    write(&path.join("wip.txt"), "uncommitted\n");
    let rev = get(&f.host, &ws.workspace_id).unwrap().revision.unwrap();
    // a revision older than what is there now
    let mut stale = rev.clone();
    stale.worktree.clear();
    assert_eq!(reason(&discard(&f.host, &apply("op-d0", &ws, &stale)).unwrap_err()), "stale_revision");
    let gone = discard(&f.host, &apply("op-d", &ws, &rev)).unwrap();
    assert_eq!(gone.workspace.state, api::WorkspaceState::Discarded);
    assert!(gone.workspace.retained_until.is_some());
    assert!(!path.exists() && f.host.root.join(".retained").join(&ws.workspace_id).join("wip.txt").exists());
    assert!(get(&f.host, &ws.workspace_id).unwrap().revision.is_none());
    // Undo while something else is there
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(reason(&undo(&f.host, &op("op-u0", &ws)).unwrap_err()), "path_taken");
    std::fs::remove_dir(&path).unwrap();
    let back = undo(&f.host, &op("op-u", &ws)).unwrap();
    assert_eq!(back.workspace.state, api::WorkspaceState::Result);
    assert_eq!(std::fs::read_to_string(path.join("wip.txt")).unwrap(), "uncommitted\n");
    // discarded again, then retention runs out
    let rev = get(&f.host, &ws.workspace_id).unwrap().revision.unwrap();
    discard(&f.host, &apply("op-d2", &ws, &rev)).unwrap();
    *f.host.now.lock().unwrap() += RETENTION + 1;
    assert_eq!(reason(&undo(&f.host, &op("op-u2", &ws)).unwrap_err()), "retention_expired");
    assert_eq!(sweep(&f.host).unwrap(), vec![ws.workspace_id.clone()]);
    assert!(!f.host.root.join(".retained").join(&ws.workspace_id).exists());
    assert_eq!(git(&f.source, &["for-each-ref", "refs/sill/"]), "");
    assert_eq!(reason(&get(&f.host, &ws.workspace_id).unwrap_err()), "not_found");
}

#[test]
fn a_merged_workspace_discards_and_undoes_to_merged() {
    let f = fixture("merged");
    let ws = worked(&f);
    let p = prepare(&f.host, &op("op-p", &ws)).unwrap();
    merge(&f.host, &apply("op-m", &ws, &p.revision)).unwrap();
    let rev = get(&f.host, &ws.workspace_id).unwrap().revision.unwrap();
    discard(&f.host, &apply("op-d", &ws, &rev)).unwrap();
    assert_eq!(undo(&f.host, &op("op-u", &ws)).unwrap().workspace.state, api::WorkspaceState::Merged);
}

#[test]
fn listing_pages_newest_first() {
    let f = fixture("list");
    for i in 0..3 {
        *f.host.now.lock().unwrap() += 10;
        create(&f.host, &create_req(&f, &format!("op-{i}"))).unwrap();
    }
    let first = list(&f.host, &api::List { after: None, limit: Some(2) }).unwrap();
    assert_eq!(first.workspaces.len(), 2);
    let rest = list(&f.host, &api::List { after: first.next.clone(), limit: Some(2) }).unwrap();
    assert_eq!(rest.workspaces.len(), 1);
    assert!(rest.next.is_none());
    assert!(first.workspaces[0].created_at >= first.workspaces[1].created_at);
    assert_eq!(reason(&list(&f.host, &api::List { after: Some("nonsense".into()), limit: None }).unwrap_err()), "invalid_cursor");
}

#[test]
fn reconcile_settles_what_a_restart_interrupted() {
    let f = fixture("reconcile");
    // a create that made its clone, then the daemon died before the row
    let src = super::super::inspect(&f.source).unwrap().unwrap();
    let at = f.host.root.join("src/half-ws-x");
    super::super::create(&src, "ws-x", "half", &at, "op-half").unwrap();
    f.host.db(|c| intent(c, "op-half", "ws-x", "create", &json!({"path": bytes(&at), "source": bytes(&f.source)}), 1)).unwrap();
    // a merge whose branch moved but whose files never followed
    let ws = worked(&f);
    let path = ws.path.to_path().unwrap();
    let old = git(&f.source, &["rev-parse", "main"]);
    let new = git(&path, &["rev-parse", "HEAD"]);
    git(&f.source, &["fetch", "-q", path.to_str().unwrap(), &format!("{}:refs/x", ws.branch)]);
    git(&f.source, &["update-ref", "refs/heads/main", &new, &old]);
    f.host.db(|c| intent(c, "op-merge", &ws.workspace_id, "merge", &json!({"branch": "main", "old": old, "new": new}), 2)).unwrap();
    reconcile(&f.host).unwrap();
    assert!(!at.exists(), "the half-made clone is gone");
    assert_eq!(git(&f.source, &["for-each-ref", "--format=%(refname)", "refs/sill/workspaces/ws-x/"]), "");
    let w = get(&f.host, &ws.workspace_id).unwrap().workspace;
    assert_eq!(w.state, api::WorkspaceState::Result, "not marked merged");
    let shown = w.operation.expect("the interrupted merge is shown");
    assert_eq!((shown.kind.as_str(), shown.state.as_str()), ("merge", "interrupted"));
    assert_eq!(git(&f.source, &["rev-parse", "main"]), new, "nothing reset to make it look complete");
    // the person finishes it by hand; a restart now finds it applied
    git(&f.source, &["read-tree", "-m", "-u", &old, &new]);
    f.host.db(|c| intent(c, "op-merge2", &ws.workspace_id, "merge", &json!({"branch": "main", "old": old, "new": new}), 3)).unwrap();
    reconcile(&f.host).unwrap();
    assert_eq!(get(&f.host, &ws.workspace_id).unwrap().workspace.state, api::WorkspaceState::Merged);
}

#[test]
fn reconcile_finishes_a_discard_whose_clone_had_moved() {
    let f = fixture("rdiscard");
    let ws = worked(&f);
    let path = ws.path.to_path().unwrap();
    let to = f.host.root.join(".retained").join(&ws.workspace_id);
    super::super::retain(&path, &f.host.root.join(".retained"), &ws.workspace_id).unwrap();
    f.host.db(|c| intent(c, "op-d", &ws.workspace_id, "discard", &json!({"from": bytes(&path), "to": bytes(&to), "prior": "result", "until": 5_000_000}), 1)).unwrap();
    reconcile(&f.host).unwrap();
    let w = get(&f.host, &ws.workspace_id).unwrap().workspace;
    assert_eq!(w.state, api::WorkspaceState::Discarded);
    assert_eq!(undo(&f.host, &op("op-u", &ws)).unwrap().workspace.state, api::WorkspaceState::Result);
}
