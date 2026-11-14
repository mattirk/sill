//! Workspace records, the operation journal and the operations the person drives (§11).
//!
//! The daemon store keeps a row per workspace and a journal entry per mutation, written before
//! any side effect. Git work runs outside the store lock ([`Host::db`] is taken only around
//! reads and writes of rows). Receipts are the shared ones (`crate::operations`): an identical
//! retry returns the workspace as it is now, never repeats the work. On startup [`reconcile`]
//! settles every unfinished entry from what is on disk; what it cannot establish stays
//! `interrupted` and is shown, never reset.
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sill_proto::sessions::HostPath;
use sill_proto::workspaces as api;

use crate::sfp::DomainError;

/// How long a discarded clone is kept for Undo (§10 item 8).
pub const RETENTION: i64 = 7 * 86_400;
/// The largest diff a `workspaces.diff` answers with.
pub const DIFF_LIMIT: usize = 4 * 1024 * 1024;

/// A session's life as far as a workspace cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Life {
    /// Deleted, or never existed.
    Missing,
    /// Ended: it will not write again.
    Ended,
    /// Not running now, but may be started again.
    Stopped,
    /// Its jail runs: it can write.
    Running,
}

/// What the daemon provides.
pub trait Host {
    /// Run `f` with the store; held only for the call.
    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;
    /// A session pinned to `pin`, not started; `create_id` makes a retry return the same one.
    fn new_session(&self, create_id: &str, agent: &str, policy: &str, pin: &Path) -> Result<String>;
    fn life(&self, session: &str) -> Life;
    /// Run `argv` in a jail with `session`'s restrictions (its next build: masks, pins, the
    /// workspace's source hidden), no network, working directory `cwd`: the workspace's own Git
    /// runs there, never on the host.
    fn run_jailed(&self, session: &str, cwd: &Path, argv: &[&str]) -> Result<std::process::Output>;
    /// Where workspaces are made; retained ones go to `<root>/.retained`.
    fn root(&self) -> PathBuf;
    fn now(&self) -> i64 {
        crate::store::now()
    }
}

/// One workspace mutation at a time across the daemon: their Git work is short, and the person
/// drives them one by one.
static OPS: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    OPS.lock().unwrap_or_else(|e| e.into_inner())
}

fn refuse(reason: &str, message: impl Into<String>, paths: Vec<String>) -> anyhow::Error {
    DomainError { code: "invalid_state", message: message.into(), details: Some(json!({"reason": reason, "paths": paths})) }.into()
}

fn not_found() -> anyhow::Error {
    DomainError { code: "not_found", message: "no such workspace".into(), details: None }.into()
}

fn key(operation_id: &str) -> String {
    Sha256::digest(operation_id.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

fn path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(b))
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, source BLOB NOT NULL, source_branch TEXT NOT NULL,
            base TEXT NOT NULL, branch TEXT NOT NULL, path BLOB NOT NULL, state TEXT NOT NULL,
            prior_state TEXT, owner TEXT, session_id TEXT, reviewed TEXT, retained BLOB,
            retained_until INTEGER, shallow INTEGER NOT NULL, created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS workspace_journal (
            operation TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, kind TEXT NOT NULL,
            intent TEXT NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
            started_at INTEGER NOT NULL);",
    )?;
    crate::operations::Journal::new(conn)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    name: String,
    source: PathBuf,
    source_branch: String,
    base: String,
    branch: String,
    path: PathBuf,
    state: String,
    prior_state: Option<String>,
    owner: Option<String>,
    session_id: Option<String>,
    reviewed: Option<api::Revision>,
    retained: Option<PathBuf>,
    retained_until: Option<i64>,
    shallow: bool,
    created_by: String,
    created_at: i64,
    updated_at: i64,
}

const COLUMNS: &str = "id,name,source,source_branch,base,branch,path,state,prior_state,owner,session_id,reviewed,retained,retained_until,shallow,created_by,created_at,updated_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    let reviewed: Option<String> = r.get(11)?;
    Ok(Row {
        id: r.get(0)?,
        name: r.get(1)?,
        source: path(r.get(2)?),
        source_branch: r.get(3)?,
        base: r.get(4)?,
        branch: r.get(5)?,
        path: path(r.get(6)?),
        state: r.get(7)?,
        prior_state: r.get(8)?,
        owner: r.get(9)?,
        session_id: r.get(10)?,
        reviewed: reviewed.and_then(|t| serde_json::from_str(&t).ok()),
        retained: r.get::<_, Option<Vec<u8>>>(12)?.map(path),
        retained_until: r.get(13)?,
        shallow: r.get(14)?,
        created_by: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

fn load(conn: &Connection, id: &str) -> Result<Option<Row>> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM workspaces WHERE id=?1"), [id], row).optional()?)
}

fn by_creator(conn: &Connection, creator: &str) -> Result<Option<Row>> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM workspaces WHERE created_by=?1"), [creator], row).optional()?)
}

fn save(conn: &Connection, r: &Row) -> Result<()> {
    conn.execute(
        &format!("INSERT OR REPLACE INTO workspaces({COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)"),
        params![
            r.id,
            r.name,
            bytes(&r.source),
            r.source_branch,
            r.base,
            r.branch,
            bytes(&r.path),
            r.state,
            r.prior_state,
            r.owner,
            r.session_id,
            r.reviewed.as_ref().map(|v| serde_json::to_string(v).unwrap()),
            r.retained.as_deref().map(bytes),
            r.retained_until,
            r.shallow,
            r.created_by,
            r.created_at,
            r.updated_at,
        ],
    )?;
    Ok(())
}

fn intent(conn: &Connection, op: &str, workspace: &str, kind: &str, intent: &Value, now: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO workspace_journal(operation,workspace_id,kind,intent,state,started_at) VALUES(?1,?2,?3,?4,'running',?5)",
        params![op, workspace, kind, intent.to_string(), now],
    )?;
    Ok(())
}

fn settle(conn: &Connection, op: &str, state: &str, detail: &str) -> Result<()> {
    conn.execute("UPDATE workspace_journal SET state=?2, detail=?3 WHERE operation=?1", params![op, state, detail])?;
    Ok(())
}

fn interrupted(conn: &Connection, workspace: &str) -> Result<Option<api::WorkspaceOperation>> {
    Ok(conn
        .query_row(
            "SELECT kind,state,detail FROM workspace_journal WHERE workspace_id=?1 AND state IN ('interrupted','conflict') ORDER BY started_at DESC LIMIT 1",
            [workspace],
            |r| Ok(api::WorkspaceOperation { kind: r.get(0)?, state: r.get(1)?, detail: r.get(2)? }),
        )
        .optional()?)
}

/// A rebase stopped part way in the clone (the person, the agent or the helper left it so).
fn rebasing(clone: &Path) -> bool {
    clone.join(".git/rebase-merge").exists() || clone.join(".git/rebase-apply").exists()
}

/// Files the clone's index holds in conflict, read through the hardened view.
fn conflicted(clone: &Path) -> Vec<String> {
    let Ok(git) = crate::changes::Git::open(clone) else { return vec![] };
    let Ok(out) = git.cmd().args(["diff", "--name-only", "--diff-filter=U", "-z"]).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// A conflict entry whose rebase has since finished or been aborted in the workspace is settled.
fn clear_conflict<H: Host>(h: &H, r: &Row) -> Result<()> {
    if !rebasing(&r.path) {
        h.db(|c| {
            c.execute("UPDATE workspace_journal SET state='done' WHERE workspace_id=?1 AND state='conflict'", [&r.id])?;
            Ok(())
        })?;
    }
    Ok(())
}

fn state(s: &str) -> api::WorkspaceState {
    match s {
        "open" => api::WorkspaceState::Open,
        "merged" => api::WorkspaceState::Merged,
        "discarded" => api::WorkspaceState::Discarded,
        _ => api::WorkspaceState::Result,
    }
}

fn record(conn: &Connection, r: &Row) -> Result<api::Workspace> {
    Ok(api::Workspace {
        workspace_id: r.id.clone(),
        name: r.name.clone(),
        source: HostPath::from_path(&r.source),
        source_branch: r.source_branch.clone(),
        base: r.base.clone(),
        branch: r.branch.clone(),
        path: HostPath::from_path(&r.path),
        state: state(&r.state),
        owner_session: r.owner.clone(),
        session_id: r.session_id.clone(),
        reviewed: r.reviewed.clone(),
        operation: interrupted(conn, &r.id)?,
        retained_until: r.retained_until.map(crate::sfp::timestamp),
        limitations: if r.shallow { vec!["shallow_source".into()] } else { vec![] },
        created_at: crate::sfp::timestamp(r.created_at),
        updated_at: crate::sfp::timestamp(r.updated_at),
    })
}

fn receipts(conn: &Connection) -> Result<crate::operations::Journal<'_>> {
    crate::operations::Journal::new(conn)
}

/// The request as its receipt fingerprints it: display text is not identity.
fn input<T: serde::Serialize>(a: &T) -> Result<Value> {
    let mut v = serde_json::to_value(a)?;
    if let Some(source) = v.get_mut("source").and_then(|s| s.as_object_mut()) {
        source.remove("display");
    }
    Ok(v)
}

fn get_row<H: Host>(h: &H, id: &str) -> Result<Row> {
    h.db(|c| load(c, id))?.ok_or_else(not_found)
}

/// An open workspace whose owner ended (or went) has no writer: it is a result (§3).
fn settle_owner<H: Host>(h: &H, r: &mut Row) -> Result<()> {
    if r.state != "open" {
        return Ok(());
    }
    let gone = r.owner.as_deref().is_none_or(|o| matches!(h.life(o), Life::Missing | Life::Ended));
    if gone {
        r.state = "result".into();
        r.owner = None;
        r.updated_at = h.now();
        h.db(|c| save(c, r))?;
    }
    Ok(())
}

fn writing<H: Host>(h: &H, r: &Row) -> bool {
    r.owner.as_deref().is_some_and(|o| h.life(o) == Life::Running)
}

/// The clone's revision now, against the source's current branch, and whether it needs a rebase.
fn revision_now(r: &Row) -> Result<(api::Revision, bool)> {
    let rev = super::revision(&r.path, &r.branch)?;
    let (branch, tip) = super::source_head(&r.source)?;
    let needs_rebase = !super::descends(&r.path, &tip, &rev.tip)?;
    Ok((api::Revision { tip: rev.tip, worktree: rev.worktree, destination_branch: branch, destination_tip: tip }, needs_rebase))
}

fn summary(r: &Row) -> Result<api::Summary> {
    let s = super::summary(&r.path, &r.branch, &r.base)?;
    Ok(api::Summary {
        commits: s.commits,
        files: s.files,
        added: s.added.to_string(),
        removed: s.removed.to_string(),
        uncommitted: s.uncommitted,
        ignored: s.ignored,
    })
}

fn changed<H: Host>(h: &H, r: &Row, operation: Option<&str>, session_id: Option<String>) -> Result<api::Changed> {
    h.db(|c| {
        Ok(api::Changed {
            workspace: record(c, r)?,
            operation: operation.map(|o| receipts(c)?.get(o)).transpose()?,
            session_id,
        })
    })
}

pub fn create<H: Host>(h: &H, a: &api::Create) -> Result<api::Created> {
    const METHOD: &str = "_sill/workspaces.create";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if let Some(operation) = h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))? {
        let r = h.db(|c| by_creator(c, &k))?.ok_or_else(|| -> anyhow::Error {
            DomainError { code: "gone", message: "the workspace this operation made is gone, or it did not complete".into(), details: None }.into()
        })?;
        return Ok(api::Created { workspace: h.db(|c| record(c, &r))?, session_id: r.session_id.clone().unwrap_or_default(), source_dirty: false, operation });
    }
    let name = a.name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(sill_proto::rpc::Error::invalid_params("name must contain 1–80 characters without control characters").into());
    }
    if a.agent.is_empty() || a.policy.is_empty() {
        return Err(sill_proto::rpc::Error::invalid_params("agent and policy are required").into());
    }
    let src = match super::inspect(&a.source.to_path().map_err(sill_proto::rpc::Error::invalid_params)?)? {
        Ok(s) => s,
        Err(refusal) => return Err(refuse(refusal.reason(), refusal.message(), vec![])),
    };
    h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?;
    let id = format!("ws-{}", crate::store::new_id());
    let at = super::path_for(&h.root(), &src, &id, name);
    let now = h.now();
    h.db(|c| intent(c, &k, &id, "create", &json!({"path": bytes(&at), "source": bytes(&src.root)}), now))?;
    let fail = |e: anyhow::Error| -> anyhow::Error {
        let _ = super::remove_partial(&at, &k);
        let _ = super::drop_anchors(&src.root, &id);
        let _ = h.db(|c| {
            settle(c, &k, "failed", &format!("{e:#}"))?;
            receipts(c)?.finish(&a.operation_id, None, Some("operation_failed"))
        });
        e
    };
    let made = super::create(&src, &id, name, &at, &k).map_err(fail)?;
    let session = h.new_session(&format!("workspace:{k}"), &a.agent, &a.policy, &made.path).map_err(fail)?;
    let r = Row {
        id: id.clone(),
        name: name.into(),
        source: src.root.clone(),
        source_branch: src.branch.clone(),
        base: made.base,
        branch: made.branch,
        path: made.path,
        state: "open".into(),
        prior_state: None,
        owner: Some(session.clone()),
        session_id: Some(session.clone()),
        reviewed: None,
        retained: None,
        retained_until: None,
        shallow: src.shallow,
        created_by: k.clone(),
        created_at: now,
        updated_at: now,
    };
    h.db(|c| {
        let tx = c.unchecked_transaction()?;
        save(c, &r)?;
        settle(c, &k, "done", "")?;
        receipts(c)?.finish(&a.operation_id, None, None)?;
        tx.commit()?;
        Ok(())
    })?;
    Ok(api::Created {
        workspace: h.db(|c| record(c, &r))?,
        session_id: session,
        source_dirty: src.dirty,
        operation: h.db(|c| receipts(c)?.get(&a.operation_id))?,
    })
}

/// Newest first; `after` is the previous page's `next`. Reading settles owners and expiries.
pub fn list<H: Host>(h: &H, a: &api::List) -> Result<api::Listed> {
    sweep(h)?;
    let limit = a.limit.unwrap_or(64);
    if !(1..=128).contains(&limit) {
        return Err(sill_proto::rpc::Error::invalid_params("limit must be 1–128").into());
    }
    let after = match &a.after {
        None => None,
        Some(cursor) => {
            let (at, id) = cursor.split_once(':').ok_or_else(|| DomainError { code: "invalid_cursor", message: "not a workspaces cursor".into(), details: None })?;
            Some((at.parse::<i64>().map_err(|_| DomainError { code: "invalid_cursor", message: "not a workspaces cursor".into(), details: None })?, id.to_string()))
        }
    };
    let mut rows = h.db(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM workspaces WHERE ?1 IS NULL OR created_at < ?1 OR (created_at = ?1 AND id < ?2) ORDER BY created_at DESC, id DESC LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(params![after.as_ref().map(|a| a.0), after.as_ref().map(|a| a.1.clone()).unwrap_or_default(), limit + 1], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let next = (rows.len() > limit as usize).then(|| {
        rows.truncate(limit as usize);
        let last = rows.last().unwrap();
        format!("{}:{}", last.created_at, last.id)
    });
    let mut workspaces = Vec::new();
    for mut r in rows {
        settle_owner(h, &mut r)?;
        workspaces.push(h.db(|c| record(c, &r))?);
    }
    Ok(api::Listed { workspaces, next })
}

pub fn get<H: Host>(h: &H, id: &str) -> Result<api::Detail> {
    let mut r = get_row(h, id)?;
    settle_owner(h, &mut r)?;
    let workspace = h.db(|c| record(c, &r))?;
    if r.state == "discarded" || !r.path.exists() {
        return Ok(api::Detail { workspace, revision: None, summary: None, needs_rebase: false });
    }
    let (revision, needs_rebase) = revision_now(&r)?;
    Ok(api::Detail { workspace, revision: Some(revision), summary: Some(summary(&r)?), needs_rebase })
}

pub fn diff<H: Host>(h: &H, a: &api::DiffRequest) -> Result<api::DiffText> {
    let r = get_row(h, &a.workspace_id)?;
    if r.state == "discarded" {
        return Err(refuse("discarded", "the workspace is discarded", vec![]));
    }
    let against = match a.against {
        api::Against::Base => r.base.clone(),
        api::Against::Destination => super::source_head(&r.source)?.1,
    };
    let (text, truncated) = super::diff(&r.path, &against, DIFF_LIMIT)?;
    Ok(api::DiffText { text, truncated })
}

pub fn finish<H: Host>(h: &H, id: &str) -> Result<api::Changed> {
    let _g = lock();
    let mut r = get_row(h, id)?;
    settle_owner(h, &mut r)?;
    if r.state == "open" {
        if writing(h, &r) {
            return Err(refuse("writer_active", "the session working in it is still running; stop it first", vec![]));
        }
        r.state = "result".into();
        r.owner = None;
        r.updated_at = h.now();
        h.db(|c| save(c, &r))?;
    }
    changed(h, &r, None, None)
}

pub fn reopen<H: Host>(h: &H, a: &api::Reopen) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.reopen";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?.is_some() {
        let r = get_row(h, &a.workspace_id)?;
        return changed(h, &r, Some(&a.operation_id), r.session_id.clone());
    }
    let mut r = get_row(h, &a.workspace_id)?;
    settle_owner(h, &mut r)?;
    if r.state != "result" && r.state != "open" {
        return Err(refuse("state", "only a result can be reopened", vec![]));
    }
    h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?;
    let live = r.session_id.clone().filter(|s| !matches!(h.life(s), Life::Missing | Life::Ended));
    let session = match live {
        Some(s) => s,
        None => {
            let (Some(agent), Some(policy)) = (a.agent.as_deref(), a.policy.as_deref()) else {
                let _ = h.db(|c| receipts(c)?.finish(&a.operation_id, None, Some("invalid_state")));
                return Err(sill_proto::rpc::Error::invalid_params("the last session ended: agent and policy are required for a new one").into());
            };
            h.new_session(&format!("workspace-reopen:{k}"), agent, policy, &r.path)?
        }
    };
    r.state = "open".into();
    r.owner = Some(session.clone());
    r.session_id = Some(session.clone());
    r.reviewed = None;
    r.updated_at = h.now();
    h.db(|c| {
        save(c, &r)?;
        receipts(c)?.finish(&a.operation_id, None, None)
    })?;
    changed(h, &r, Some(&a.operation_id), Some(session))
}

/// Record the destination (anchored, and visible in the clone) and the revision a Merge would
/// apply. Repeatable: a retry recomputes.
pub fn prepare<H: Host>(h: &H, a: &api::Operate) -> Result<api::Prepared> {
    const METHOD: &str = "_sill/workspaces.prepare";
    let fingerprint = input(a)?;
    let _g = lock();
    let mut r = get_row(h, &a.workspace_id)?;
    settle_owner(h, &mut r)?;
    if r.state == "open" {
        return Err(refuse("writer_active", "a session is working in it; finish it first", vec![]));
    }
    if r.state != "result" {
        return Err(refuse("state", "only a result can be merged", vec![]));
    }
    clear_conflict(h, &r)?;
    if rebasing(&r.path) {
        return Err(refuse("rebase_in_progress", "A rebase is stopped in the workspace: finish it there (Reopen) or abort it.", conflicted(&r.path)));
    }
    let (branch, _) = super::source_head(&r.source)?;
    if branch.is_empty() {
        return Err(refuse("detached_head", "check out a branch in the source first", vec![]));
    }
    // what a rebase would need, checked before anything is recorded or moved
    let (current, needs_rebase) = revision_now(&r)?;
    let mut helper = None;
    if needs_rebase {
        if !current.worktree.is_empty() {
            return Err(refuse("uncommitted", "The workspace has uncommitted changes; a rebase needs them committed or removed.", super::summary(&r.path, &r.branch, &r.base)?.uncommitted));
        }
        let Some(session) = r.session_id.clone().filter(|s| h.life(s) != Life::Missing) else {
            return Err(refuse("no_session", "The workspace's session is gone; reopen it to rebase.", vec![]));
        };
        helper = Some(session);
    }
    let fresh = h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?.1;
    let dest = super::set_destination(&r.source, &r.path, &r.id, &branch, &r.branch)?;
    let mut conflicts = Vec::new();
    if let Some(session) = helper {
        let out = h.run_jailed(&session, &r.path, &["git", "rebase", "--no-autostash", "refs/sill/dest"])?;
        if !out.status.success() {
            if rebasing(&r.path) {
                conflicts = conflicted(&r.path);
                let detail = format!("The rebase onto {branch} ({}) stopped: {}", &dest[..dest.len().min(12)], if conflicts.is_empty() { "see the workspace".into() } else { conflicts.join(", ") });
                let k = key(&a.operation_id);
                h.db(|c| {
                    intent(c, &k, &r.id, "rebase", &json!({"dest": dest}), h.now())?;
                    settle(c, &k, "conflict", &detail)
                })?;
            } else {
                let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let hint = if r.shallow { " The source is shallow: fetch more history in it, then prepare again." } else { "" };
                if fresh {
                    h.db(|c| receipts(c)?.finish(&a.operation_id, None, Some("invalid_state")))?;
                }
                return Err(refuse("rebase_failed", format!("The rebase could not run: {why}.{hint}"), vec![]));
            }
        }
    }
    let (revision, needs_rebase) = revision_now(&r)?;
    let changed = r.reviewed.as_ref() != Some(&revision);
    r.reviewed = Some(revision.clone());
    r.updated_at = h.now();
    h.db(|c| {
        save(c, &r)?;
        if fresh {
            receipts(c)?.finish(&a.operation_id, None, None)?;
        }
        Ok(())
    })?;
    Ok(api::Prepared { workspace: h.db(|c| record(c, &r))?, revision, changed, needs_rebase, conflicts })
}

/// Abort a rebase that stopped in the workspace, in the same helper jail.
pub fn abort<H: Host>(h: &H, a: &api::Operate) -> Result<api::Changed> {
    let _g = lock();
    let r = get_row(h, &a.workspace_id)?;
    if !rebasing(&r.path) {
        clear_conflict(h, &r)?;
        return changed(h, &r, None, None);
    }
    if writing(h, &r) {
        return Err(refuse("writer_active", "a session is working in it; abort the rebase there", vec![]));
    }
    let Some(session) = r.session_id.clone().filter(|s| h.life(s) != Life::Missing) else {
        return Err(refuse("no_session", "The workspace's session is gone; reopen it to abort the rebase there.", vec![]));
    };
    let out = h.run_jailed(&session, &r.path, &["git", "rebase", "--abort"])?;
    if !out.status.success() {
        return Err(refuse("rebase_failed", format!("The rebase could not be aborted: {}", String::from_utf8_lossy(&out.stderr).trim()), vec![]));
    }
    clear_conflict(h, &r)?;
    changed(h, &r, None, None)
}

fn stop_message(stop: &super::Stop) -> String {
    match stop {
        super::Stop::SourceMoved => "The source's branch moved or changed since this was prepared; prepare again.".into(),
        super::Stop::NeedsPrepare => "The result does not include the source's latest commits: rebase it in the workspace first.".into(),
        super::Stop::Overlap(p) => format!("Your uncommitted edits touch files the result changes: {}", p.join(", ")),
        super::Stop::Untracked(p) => format!("Untracked files in the source would be overwritten: {}", p.join(", ")),
    }
}

pub fn merge<H: Host>(h: &H, a: &api::Apply) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.merge";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?.is_some() {
        let r = get_row(h, &a.workspace_id)?;
        return changed(h, &r, Some(&a.operation_id), None);
    }
    let mut r = get_row(h, &a.workspace_id)?;
    settle_owner(h, &mut r)?;
    if r.state == "open" {
        return Err(refuse("writer_active", "a session is working in it; finish it first", vec![]));
    }
    if r.state != "result" {
        return Err(refuse("state", "only a result can be merged", vec![]));
    }
    if rebasing(&r.path) {
        return Err(refuse("rebase_in_progress", "A rebase is stopped in the workspace: finish it there (Reopen) or abort it.", conflicted(&r.path)));
    }
    let (now_rev, _) = revision_now(&r)?;
    if now_rev != a.revision {
        return Err(refuse("stale_revision", "The workspace or the source changed since you looked; review it again.", vec![]));
    }
    if !now_rev.worktree.is_empty() {
        let s = super::summary(&r.path, &r.branch, &r.base)?;
        return Err(refuse("uncommitted", "The workspace has uncommitted changes, which a Merge would leave behind; commit or remove them.", s.uncommitted));
    }
    if now_rev.destination_branch.is_empty() {
        return Err(refuse("detached_head", "check out a branch in the source first", vec![]));
    }
    h.db(|c| {
        receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint)?;
        intent(c, &k, &r.id, "merge", &json!({"branch": now_rev.destination_branch, "old": now_rev.destination_tip, "new": now_rev.tip}), h.now())
    })?;
    let outcome = super::fast_forward(&r.source, &r.path, &r.id, &now_rev.destination_branch, &now_rev.destination_tip, &r.branch);
    match outcome {
        Ok(Ok(_)) => {
            r.state = "merged".into();
            r.updated_at = h.now();
            h.db(|c| {
                let tx = c.unchecked_transaction()?;
                save(c, &r)?;
                settle(c, &k, "done", "")?;
                receipts(c)?.finish(&a.operation_id, None, None)?;
                tx.commit()?;
                Ok(())
            })?;
            changed(h, &r, Some(&a.operation_id), None)
        }
        Ok(Err(stop)) => {
            let message = stop_message(&stop);
            h.db(|c| {
                settle(c, &k, "failed", &message)?;
                receipts(c)?.finish(&a.operation_id, None, Some("invalid_state"))
            })?;
            Err(refuse(stop.reason(), message, stop.paths().to_vec()))
        }
        Err(e) => {
            // a Git failure part way: what happened is for reconcile to establish
            let detail = after_merge(&r, &now_rev.destination_branch, &now_rev.destination_tip, &now_rev.tip, h, &k)?;
            let _ = h.db(|c| receipts(c)?.finish(&a.operation_id, None, Some("operation_failed")));
            Err(e.context(detail))
        }
    }
}

/// Settle a merge whose outcome is uncertain from the source as it is: applied, not applied,
/// or interrupted (shown, never reset).
fn after_merge<H: Host>(r: &Row, branch: &str, old: &str, new: &str, h: &H, k: &str) -> Result<String> {
    let applied = super::merge_applied(&r.source, branch, old, new).unwrap_or(super::Applied::Unclear);
    let mut r = r.clone();
    let detail = match applied {
        super::Applied::Yes => {
            r.state = "merged".into();
            r.updated_at = h.now();
            h.db(|c| {
                save(c, &r)?;
                settle(c, k, "done", "")
            })?;
            "the merge had taken effect".to_string()
        }
        super::Applied::No => {
            h.db(|c| settle(c, k, "failed", "the merge did not take effect"))?;
            "the merge did not take effect".to_string()
        }
        super::Applied::Unclear => {
            let d = format!("The Merge was interrupted: the source's {branch} should be at {old} (before) or {new} (after) with its files; check the source before going on.");
            h.db(|c| settle(c, k, "interrupted", &d))?;
            d
        }
    };
    Ok(detail)
}

pub fn discard<H: Host>(h: &H, a: &api::Apply) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.discard";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?.is_some() {
        let r = get_row(h, &a.workspace_id)?;
        return changed(h, &r, Some(&a.operation_id), None);
    }
    let mut r = get_row(h, &a.workspace_id)?;
    settle_owner(h, &mut r)?;
    if r.state == "discarded" {
        return Err(refuse("state", "already discarded", vec![]));
    }
    if writing(h, &r) {
        return Err(refuse("writer_active", "the session working in it is still running; stop it first", vec![]));
    }
    let now_rev = super::revision(&r.path, &r.branch)?;
    if now_rev.tip != a.revision.tip || now_rev.worktree != a.revision.worktree {
        return Err(refuse("stale_revision", "The workspace changed since you looked; review what would be lost again.", vec![]));
    }
    let to = h.root().join(".retained").join(&r.id);
    let until = h.now() + RETENTION;
    h.db(|c| {
        receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint)?;
        intent(c, &k, &r.id, "discard", &json!({"from": bytes(&r.path), "to": bytes(&to), "prior": r.state, "until": until}), h.now())
    })?;
    if let Err(e) = super::retain(&r.path, &h.root().join(".retained"), &r.id) {
        let _ = h.db(|c| {
            settle(c, &k, "failed", &format!("{e:#}"))?;
            receipts(c)?.finish(&a.operation_id, None, Some("operation_failed"))
        });
        return Err(e);
    }
    discarded(&mut r, to, until, h.now());
    h.db(|c| {
        let tx = c.unchecked_transaction()?;
        save(c, &r)?;
        settle(c, &k, "done", "")?;
        receipts(c)?.finish(&a.operation_id, None, None)?;
        tx.commit()?;
        Ok(())
    })?;
    changed(h, &r, Some(&a.operation_id), None)
}

fn discarded(r: &mut Row, to: PathBuf, until: i64, now: i64) {
    r.prior_state = Some(if r.state == "merged" { "merged".into() } else { "result".into() });
    r.state = "discarded".into();
    r.owner = None;
    r.retained = Some(to);
    r.retained_until = Some(until);
    r.reviewed = None;
    r.updated_at = now;
}

pub fn undo<H: Host>(h: &H, a: &api::Operate) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.undo";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?.is_some() {
        let r = get_row(h, &a.workspace_id)?;
        return changed(h, &r, Some(&a.operation_id), None);
    }
    let mut r = get_row(h, &a.workspace_id)?;
    if r.state != "discarded" {
        return Err(refuse("state", "only a discarded workspace can be restored", vec![]));
    }
    let (Some(retained), Some(until)) = (r.retained.clone(), r.retained_until) else {
        return Err(refuse("retention_expired", "nothing is retained", vec![]));
    };
    if h.now() > until || !retained.exists() {
        return Err(refuse("retention_expired", "the retained copy is gone", vec![]));
    }
    if r.path.exists() {
        return Err(refuse("path_taken", format!("something else is at {} now", r.path.display()), vec![r.path.display().to_string()]));
    }
    h.db(|c| {
        receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint)?;
        intent(c, &k, &r.id, "undo", &json!({"from": bytes(&retained), "to": bytes(&r.path)}), h.now())
    })?;
    if !super::restore(&retained, &r.path)? {
        h.db(|c| {
            settle(c, &k, "failed", "path taken")?;
            receipts(c)?.finish(&a.operation_id, None, Some("invalid_state"))
        })?;
        return Err(refuse("path_taken", format!("something else is at {} now", r.path.display()), vec![r.path.display().to_string()]));
    }
    restored(&mut r, h.now());
    h.db(|c| {
        let tx = c.unchecked_transaction()?;
        save(c, &r)?;
        settle(c, &k, "done", "")?;
        receipts(c)?.finish(&a.operation_id, None, None)?;
        tx.commit()?;
        Ok(())
    })?;
    changed(h, &r, Some(&a.operation_id), None)
}

fn restored(r: &mut Row, now: i64) {
    r.state = r.prior_state.take().unwrap_or_else(|| "result".into());
    r.retained = None;
    r.retained_until = None;
    r.updated_at = now;
}

/// The source repository `session` works on as a workspace, when it is one: the latest row naming
/// it as owner or session. None in a store without workspaces.
pub fn session_source(conn: &Connection, session: &str) -> Result<Option<PathBuf>> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workspaces')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT source FROM workspaces WHERE owner=?1 OR session_id=?1 ORDER BY updated_at DESC LIMIT 1",
            [session],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .map(path))
}

/// What the agent of a workspace session is told (§5): where it works, on which branch from which
/// base, that the source checkout is hidden, and that the person merges. None for other sessions.
pub fn session_notice(conn: &Connection, session: &str) -> Result<Option<String>> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workspaces')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let r = conn
        .query_row(&format!("SELECT {COLUMNS} FROM workspaces WHERE owner=?1 OR session_id=?1 ORDER BY updated_at DESC LIMIT 1"), [session], row)
        .optional()?;
    Ok(r.filter(|r| r.state != "discarded").map(|r| {
        format!(
            "\n[This session works in a workspace: {path}, its own clone of {source} on branch {branch}, started from {base}. The source checkout is hidden from this jail — work and commit here; uncommitted work is not merged. The person reviews the result and merges it into the source, or discards it.]",
            path = r.path.display(),
            source = r.source.display(),
            branch = r.branch,
            base = &r.base[..r.base.len().min(12)],
        )
    }))
}

/// The end of retention: retained clones past their time go, with their anchors and rows.
pub fn sweep<H: Host>(h: &H) -> Result<Vec<String>> {
    let now = h.now();
    let due = h.db(|c| {
        let mut stmt = c.prepare(&format!("SELECT {COLUMNS} FROM workspaces WHERE state='discarded' AND retained_until < ?1"))?;
        let rows = stmt.query_map([now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let mut gone = Vec::new();
    for r in due {
        let _g = lock();
        if let Some(retained) = &r.retained {
            super::expire(retained, &r.source, &r.id).with_context(|| format!("expire {}", r.id))?;
        } else if r.source.exists() {
            super::drop_anchors(&r.source, &r.id)?;
        }
        h.db(|c| {
            c.execute("DELETE FROM workspaces WHERE id=?1", [&r.id])?;
            Ok(())
        })?;
        gone.push(r.id);
    }
    Ok(gone)
}

/// At daemon start, before serving: settle every unfinished journal entry from what is on disk
/// (§11 journal table), then expire what is due.
pub fn reconcile<H: Host>(h: &H) -> Result<()> {
    let _g = lock();
    let running: Vec<(String, String, String, String)> = h.db(|c| {
        let mut stmt = c.prepare("SELECT operation,workspace_id,kind,intent FROM workspace_journal WHERE state='running'")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    for (op, id, kind, intent_text) in running {
        let i: Value = serde_json::from_str(&intent_text).unwrap_or(Value::Null);
        let p = |name: &str| -> PathBuf {
            path(i[name].as_array().map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default())
        };
        let row = h.db(|c| load(c, &id))?;
        match kind.as_str() {
            "create" => {
                if h.db(|c| by_creator(c, &op))?.is_some() {
                    h.db(|c| settle(c, &op, "done", ""))?;
                } else {
                    let _ = super::remove_partial(&p("path"), &op);
                    let source = p("source");
                    if source.exists() {
                        let _ = super::drop_anchors(&source, &id);
                    }
                    h.db(|c| settle(c, &op, "failed", "interrupted by a restart; nothing was kept"))?;
                }
            }
            "merge" => {
                let Some(r) = row else {
                    h.db(|c| settle(c, &op, "failed", "the workspace is gone"))?;
                    continue;
                };
                let s = |k: &str| i[k].as_str().unwrap_or_default().to_string();
                after_merge(&r, &s("branch"), &s("old"), &s("new"), h, &op)?;
            }
            "discard" => {
                let (from, to) = (p("from"), p("to"));
                match row {
                    Some(mut r) if to.exists() && !from.exists() => {
                        r.state = i["prior"].as_str().unwrap_or("result").into();
                        discarded(&mut r, to, i["until"].as_i64().unwrap_or(h.now() + RETENTION), h.now());
                        h.db(|c| {
                            save(c, &r)?;
                            settle(c, &op, "done", "")
                        })?;
                    }
                    _ => h.db(|c| settle(c, &op, "failed", "interrupted before the clone moved"))?,
                }
            }
            "undo" => {
                let (from, to) = (p("from"), p("to"));
                match row {
                    Some(mut r) if to.exists() && !from.exists() => {
                        restored(&mut r, h.now());
                        h.db(|c| {
                            save(c, &r)?;
                            settle(c, &op, "done", "")
                        })?;
                    }
                    _ => h.db(|c| settle(c, &op, "failed", "interrupted before the clone moved back"))?,
                }
            }
            _ => h.db(|c| settle(c, &op, "failed", "unknown operation"))?,
        }
    }
    drop(_g);
    sweep(h)?;
    Ok(())
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
