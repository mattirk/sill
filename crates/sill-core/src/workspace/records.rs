//! Workspace records, the operation journal and what the person does (docs/parallel-coding.md
//! §12): a workspace per session and pinned repository, made when the session's jail starts;
//! Apply and Discard as often as the person likes; Undo of discarded work for 7 days; a finished
//! folder removed.
//!
//! The daemon store keeps a row per workspace, a row per discarded piece of work, the session's
//! mode and the repositories it keeps live, and a journal entry per mutation written before any
//! side effect. Git work runs outside the store lock ([`Host::db`] is held only around rows).
//! Receipts are the shared ones (`crate::operations`): an identical retry never repeats work. On
//! start [`reconcile`] settles unfinished entries from what is on disk; what it cannot establish
//! stays `interrupted` and is shown, never reset.
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sill_proto::sessions::HostPath;
use sill_proto::workspaces as api;

use super::{carry, flow};
use crate::sfp::DomainError;

/// How long discarded work is kept for Undo (§10 item 8).
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
    /// Not ended: its jail may run or be started again.
    Alive,
}

/// What the daemon provides.
pub trait Host {
    /// Run `f` with the store; held only for the call.
    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;
    fn life(&self, session: &str) -> Life;
    /// The agent is in a turn: its files may change any moment.
    fn busy(&self, session: &str) -> bool;
    /// The session's own pinned folders (not the global ones).
    fn session_pins(&self, session: &str) -> Result<Vec<PathBuf>>;
    /// Run `argv` in a jail with `session`'s restrictions (its next build, the workspace at the
    /// repository's path), no network, working directory `cwd`: the workspace's own Git runs
    /// there, never on the host.
    fn run_jailed(&self, session: &str, cwd: &Path, argv: &[&str]) -> Result<std::process::Output>;
    /// Where workspaces are made.
    fn root(&self) -> PathBuf;
    /// The session's overlay as its last jail had it, read while its jail is down: None when it
    /// never ran (§12 work from before the pin).
    fn overlay(&self, _session: &str) -> Result<Option<Overlay>> {
        Ok(None)
    }
    fn now(&self) -> i64 {
        crate::store::now()
    }
}

/// A session's overlay: each layer's host path and upper tree, and when the session began (unix
/// seconds).
#[derive(Debug, Clone)]
pub struct Overlay {
    pub layers: Vec<(PathBuf, PathBuf)>,
    pub started: i64,
}

impl Overlay {
    /// Where `source` lies in the upper trees: the deepest layer holding it.
    fn upper_of(&self, source: &Path) -> Option<PathBuf> {
        self.layers
            .iter()
            .filter(|(host, _)| source.starts_with(host))
            .max_by_key(|(host, _)| host.components().count())
            .map(|(host, upper)| upper.join(source.strip_prefix(host).expect("below its layer")))
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

fn not_found(what: &str) -> anyhow::Error {
    DomainError { code: "not_found", message: format!("no such {what}"), details: None }.into()
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
    // a store from the first design (2026-11-09, before §12) holds no workspace worth keeping
    let old: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workspaces')
           AND NOT EXISTS(SELECT 1 FROM pragma_table_info('workspaces') WHERE name='discards')",
        [],
        |r| r.get(0),
    )?;
    if old {
        conn.execute_batch("DROP TABLE workspaces; DROP TABLE IF EXISTS workspace_journal;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, session_id TEXT NOT NULL, source BLOB NOT NULL,
            source_branch TEXT NOT NULL, base TEXT NOT NULL, branch TEXT NOT NULL, path BLOB NOT NULL,
            state TEXT NOT NULL, shallow INTEGER NOT NULL, discards INTEGER NOT NULL,
            created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            UNIQUE(session_id, source));
         CREATE TABLE IF NOT EXISTS workspace_discards (
            id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, session_id TEXT NOT NULL, name TEXT NOT NULL,
            source BLOB NOT NULL, branch TEXT NOT NULL, snapshot TEXT NOT NULL, anchor TEXT NOT NULL,
            until INTEGER NOT NULL, created_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS workspace_journal (
            operation TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, kind TEXT NOT NULL,
            intent TEXT NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
            started_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS session_workspace_mode (session_id TEXT PRIMARY KEY, workspaces INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS session_live_repositories (
            session_id TEXT NOT NULL, source BLOB NOT NULL, reason TEXT NOT NULL, PRIMARY KEY(session_id, source));",
    )?;
    crate::operations::Journal::new(conn)?;
    Ok(())
}

fn has_tables(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_workspace_mode')", [], |r| r.get(0))?)
}

/// Record whether `session` works in workspaces (fixed when it is made, §12).
pub fn set_mode(conn: &Connection, session: &str, workspaces: bool) -> Result<()> {
    init(conn)?;
    conn.execute(
        "INSERT INTO session_workspace_mode(session_id, workspaces) VALUES(?1, ?2) ON CONFLICT(session_id) DO UPDATE SET workspaces=excluded.workspaces",
        params![session, workspaces],
    )?;
    Ok(())
}

pub fn mode(conn: &Connection, session: &str) -> Result<bool> {
    if !has_tables(conn)? {
        return Ok(false);
    }
    Ok(conn.query_row("SELECT workspaces FROM session_workspace_mode WHERE session_id=?1", [session], |r| r.get(0)).optional()?.unwrap_or(false))
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    name: String,
    session_id: String,
    source: PathBuf,
    source_branch: String,
    base: String,
    branch: String,
    path: PathBuf,
    state: String,
    shallow: bool,
    discards: i64,
    created_by: String,
    created_at: i64,
    updated_at: i64,
}

const COLUMNS: &str = "id,name,session_id,source,source_branch,base,branch,path,state,shallow,discards,created_by,created_at,updated_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        name: r.get(1)?,
        session_id: r.get(2)?,
        source: path(r.get(3)?),
        source_branch: r.get(4)?,
        base: r.get(5)?,
        branch: r.get(6)?,
        path: path(r.get(7)?),
        state: r.get(8)?,
        shallow: r.get(9)?,
        discards: r.get(10)?,
        created_by: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

fn load(conn: &Connection, id: &str) -> Result<Option<Row>> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM workspaces WHERE id=?1"), [id], row).optional()?)
}

fn of_session(conn: &Connection, session: &str) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM workspaces WHERE session_id=?1 ORDER BY created_at"))?;
    let rows = stmt.query_map([session], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn save(conn: &Connection, r: &Row) -> Result<()> {
    conn.execute(
        &format!("INSERT OR REPLACE INTO workspaces({COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"),
        params![
            r.id,
            r.name,
            r.session_id,
            bytes(&r.source),
            r.source_branch,
            r.base,
            r.branch,
            bytes(&r.path),
            r.state,
            r.shallow,
            r.discards,
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

/// A note for the person (a carry taken without a base) has been seen through once they Apply or
/// Discard.
fn settle_notices(conn: &Connection, workspace: &str) -> Result<()> {
    conn.execute("UPDATE workspace_journal SET state='done' WHERE workspace_id=?1 AND state='notice'", [workspace])?;
    Ok(())
}

fn shown_operation(conn: &Connection, workspace: &str) -> Result<Option<api::WorkspaceOperation>> {
    Ok(conn
        .query_row(
            "SELECT kind,state,detail FROM workspace_journal WHERE workspace_id=?1 AND state IN ('interrupted','conflict','waiting','notice') ORDER BY started_at DESC LIMIT 1",
            [workspace],
            |r| Ok(api::WorkspaceOperation { kind: r.get(0)?, state: r.get(1)?, detail: r.get(2)? }),
        )
        .optional()?)
}

fn record(conn: &Connection, r: &Row) -> Result<api::Workspace> {
    Ok(api::Workspace {
        workspace_id: r.id.clone(),
        name: r.name.clone(),
        session_id: r.session_id.clone(),
        source: HostPath::from_path(&r.source),
        source_branch: r.source_branch.clone(),
        base: r.base.clone(),
        branch: r.branch.clone(),
        path: HostPath::from_path(&r.path),
        state: if r.state == "result" { api::WorkspaceState::Result } else { api::WorkspaceState::Open },
        operation: shown_operation(conn, &r.id)?,
        limitations: if r.shallow { vec!["shallow_source".into()] } else { vec![] },
        created_at: crate::sfp::timestamp(r.created_at),
        updated_at: crate::sfp::timestamp(r.updated_at),
    })
}

struct Discard {
    id: String,
    workspace_id: String,
    session_id: String,
    name: String,
    source: PathBuf,
    branch: String,
    snapshot: String,
    anchor: String,
    until: i64,
    created_at: i64,
}

fn discard_row(r: &rusqlite::Row) -> rusqlite::Result<Discard> {
    Ok(Discard {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        session_id: r.get(2)?,
        name: r.get(3)?,
        source: path(r.get(4)?),
        branch: r.get(5)?,
        snapshot: r.get(6)?,
        anchor: r.get(7)?,
        until: r.get(8)?,
        created_at: r.get(9)?,
    })
}

const DISCARD_COLUMNS: &str = "id,workspace_id,session_id,name,source,branch,snapshot,anchor,until,created_at";

fn discard_record(d: &Discard) -> api::Discarded {
    api::Discarded {
        discard_id: d.id.clone(),
        workspace_id: d.workspace_id.clone(),
        name: d.name.clone(),
        source: HostPath::from_path(&d.source),
        until: crate::sfp::timestamp(d.until),
        created_at: crate::sfp::timestamp(d.created_at),
    }
}

fn receipts(conn: &Connection) -> Result<crate::operations::Journal<'_>> {
    crate::operations::Journal::new(conn)
}

fn input<T: serde::Serialize>(a: &T) -> Result<Value> {
    Ok(serde_json::to_value(a)?)
}

fn get_row<H: Host>(h: &H, id: &str) -> Result<Row> {
    h.db(|c| load(c, id))?.ok_or_else(|| not_found("workspace"))
}

/// A stopped rebase in the clone (the agent, the person or the helper left it so).
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

/// The workspace is done with: its folder goes (nothing in it is left to apply, and the next
/// workspace takes its build output from the checkout, §12), its anchors go except those of
/// discarded work, its row goes.
fn free<H: Host>(h: &H, r: &Row) -> Result<()> {
    if r.path.exists() {
        super::remove_folder(&r.path).with_context(|| format!("remove {}", r.path.display()))?;
    }
    if r.source.exists() {
        for name in ["base", "dest", "incoming"] {
            let _ = drop_ref(&r.source, &super::anchor_ref(&r.id, name));
        }
    }
    h.db(|c| {
        c.execute("DELETE FROM workspaces WHERE id=?1", [&r.id])?;
        Ok(())
    })
}

fn drop_ref(repo: &Path, name: &str) -> Result<()> {
    let git = crate::changes::Git::open(repo)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-d", name]);
    c.output()?;
    Ok(())
}

/// One look at a workspace (§12): the person's side read from their checkout, serving the
/// settling and the summary alike, so a request looks once. None when the folder or the checkout
/// is gone: nothing can be read, and the work is kept.
fn look(r: &Row) -> Result<Option<flow::Mirror>> {
    if !r.path.exists() || !r.source.exists() {
        return Ok(None);
    }
    flow::mirror(&r.source, &r.path, &r.branch).map(Some)
}

/// Whether the workspace is kept rather than freed: work to apply, a stopped rebase, or nothing
/// readable.
fn keeps_work(r: &Row, l: &Option<flow::Mirror>) -> bool {
    rebasing(&r.path) || l.as_ref().is_none_or(|m| m.changed)
}

/// An open workspace whose session has ended.
fn ending<H: Host>(h: &H, r: &Row) -> bool {
    r.state == "open" && matches!(h.life(&r.session_id), Life::Missing | Life::Ended)
}

/// A workspace whose session ended: with work left, a result; with nothing, freed (§12).
/// Returns the row still there.
fn settle_session<H: Host>(h: &H, r: Row) -> Result<Option<Row>> {
    if !ending(h, &r) {
        return Ok(Some(r));
    }
    let l = look(&r)?;
    settle_ended(h, r, &l)
}

fn settle_ended<H: Host>(h: &H, mut r: Row, l: &Option<flow::Mirror>) -> Result<Option<Row>> {
    if !keeps_work(&r, l) {
        free(h, &r)?;
        return Ok(None);
    }
    r.state = "result".into();
    r.updated_at = h.now();
    h.db(|c| save(c, &r))?;
    Ok(Some(r))
}

/// A workspace whose pin was removed: freed when nothing is left to apply, else a result.
fn settle_unpinned<H: Host>(h: &H, mut r: Row) -> Result<()> {
    if !keeps_work(&r, &look(&r)?) {
        return free(h, &r);
    }
    r.state = "result".into();
    r.updated_at = h.now();
    h.db(|c| save(c, &r))
}

/// After an Apply or Discard of a result: nothing left, so it is done with.
fn settle_result<H: Host>(h: &H, r: Row) -> Result<Option<Row>> {
    if r.state == "result" && !keeps_work(&r, &look(&r)?) {
        free(h, &r)?;
        return Ok(None);
    }
    Ok(Some(r))
}

/// A repository's top: a folder with its own `.git` directory or file.
fn repository_top(p: &Path) -> bool {
    p.is_dir() && p.join(".git").exists()
}

/// Where a workspace session's jail puts its clone (§12): at the repository's own path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub source: PathBuf,
    pub clone: PathBuf,
    /// The source's own object store, borrowed read-only.
    pub objects: PathBuf,
}

/// Every workspace folder there is, of any session: sill's own clones, where the window's live
/// diff and revert work as in a pinned repository (the drawer of a workspace session reads its
/// clone, gap 1).
pub fn all_clones(conn: &Connection) -> Result<Vec<PathBuf>> {
    if !has_tables(conn)? {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM workspaces"))?;
    let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(|r| r.path).filter(|p| p.exists()).collect())
}

/// Every workspace of `session` whose folder is there, open or a result waiting for the person:
/// what the rebase helper binds, since a result's rebase runs in its clone too.
pub fn session_layout(conn: &Connection, session: &str) -> Result<Vec<Layout>> {
    layout_where(conn, session, |_| true)
}

/// The workspaces `session`'s jail binds: the open ones, those of its pinned repositories at its
/// last start. A workspace whose pin was removed is a result and is not mounted.
pub fn session_mounts(conn: &Connection, session: &str) -> Result<Vec<Layout>> {
    layout_where(conn, session, |r| r.state == "open")
}

fn layout_where(conn: &Connection, session: &str, keep: impl Fn(&Row) -> bool) -> Result<Vec<Layout>> {
    if !has_tables(conn)? {
        return Ok(vec![]);
    }
    Ok(of_session(conn, session)?
        .into_iter()
        .filter(|r| r.path.exists() && keep(r))
        .map(|r| Layout { objects: r.source.join(".git/objects"), source: r.source, clone: r.path })
        .collect())
}

/// At the start of `session`'s jail (§12), the workspaces made to match its pins: one for each
/// session-pinned repository, made from the checkout (warm where it can be), or kept from the
/// session's last start. A repository that cannot have one stays live, its reason recorded. A
/// workspace whose repository is no longer pinned leaves the jail and settles as at the session's
/// end: with nothing left to apply its folder goes, with work waiting a result on Changes
/// (pinned again, it opens again). Nothing for a live session.
pub fn ensure_for_session<H: Host>(h: &H, session: &str) -> Result<Vec<Layout>> {
    if !h.db(|c| mode(c, session))? {
        return Ok(vec![]);
    }
    let _g = lock();
    let mut layout = Vec::new();
    let pins = h.session_pins(session)?;
    // read before any jail of the session runs again: the work it left over the repositories
    let overlay = h.overlay(session).unwrap_or_else(|e| {
        eprintln!("sill: workspaces of {session}: the overlay could not be read, nothing carried: {e:#}");
        None
    });
    let pinned: Vec<PathBuf> = pins.iter().filter(|p| repository_top(p)).filter_map(|p| p.canonicalize().ok()).collect();
    for r in h.db(|c| of_session(c, session))? {
        if r.state == "open" && !pinned.contains(&r.source) {
            settle_unpinned(h, r)?;
        }
    }
    // a refused repository unpinned: its reason goes with it
    h.db(|c| {
        let mut stmt = c.prepare("SELECT source FROM session_live_repositories WHERE session_id=?1")?;
        let live = stmt.query_map([session], |r| r.get::<_, Vec<u8>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for source in live.into_iter().filter(|s| !pinned.iter().any(|p| bytes(p) == *s)) {
            c.execute("DELETE FROM session_live_repositories WHERE session_id=?1 AND source=?2", params![session, source])?;
        }
        Ok(())
    })?;
    for source in pinned {
        let existing = h.db(|c| {
            Ok(c.query_row(&format!("SELECT {COLUMNS} FROM workspaces WHERE session_id=?1 AND source=?2"), params![session, bytes(&source)], row).optional()?)
        })?;
        if let Some(mut r) = existing.filter(|r| r.path.exists()) {
            // before the jail runs in it: what the looks since its last start left goes
            let _ = flow::prune(&r.path);
            if r.state != "open" {
                r.state = "open".into();
                r.updated_at = h.now();
                h.db(|c| save(c, &r))?;
            }
            carry_into(h, &r, overlay.as_ref());
            layout.push(Layout { objects: r.source.join(".git/objects"), source: r.source, clone: r.path });
            continue;
        }
        let src = match super::inspect(&source)? {
            Ok(s) => s,
            Err(refusal) => {
                h.db(|c| {
                    c.execute(
                        "INSERT OR REPLACE INTO session_live_repositories(session_id, source, reason) VALUES(?1,?2,?3)",
                        params![session, bytes(&source), refusal.reason()],
                    )?;
                    Ok(())
                })?;
                continue;
            }
        };
        let id = format!("ws-{}", crate::store::new_id());
        let name = src.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "work".into());
        let at = super::path_for(&h.root(), &src, &id, &name);
        let op = key(&format!("ensure:{session}:{id}"));
        let now = h.now();
        h.db(|c| intent(c, &op, &id, "create", &json!({"path": bytes(&at), "source": bytes(&src.root)}), now))?;
        let made = match super::create(&src, &id, &name, &at, &op) {
            Ok(m) => m,
            Err(e) => {
                let _ = super::remove_partial(&at, &op);
                let _ = super::drop_anchors(&src.root, &id);
                h.db(|c| settle(c, &op, "failed", &format!("{e:#}")))?;
                return Err(e.context(format!("make the workspace of {}", src.root.display())));
            }
        };
        let r = Row {
            id: id.clone(),
            name,
            session_id: session.into(),
            source: src.root.clone(),
            source_branch: src.branch.clone(),
            base: made.base,
            branch: made.branch,
            path: made.path,
            state: "open".into(),
            shallow: src.shallow,
            discards: 0,
            created_by: op.clone(),
            created_at: now,
            updated_at: now,
        };
        h.db(|c| {
            let tx = c.unchecked_transaction()?;
            save(c, &r)?;
            settle(c, &op, "done", "")?;
            c.execute("DELETE FROM session_live_repositories WHERE session_id=?1 AND source=?2", params![session, bytes(&src.root)])?;
            tx.commit()?;
            Ok(())
        })?;
        carry_into(h, &r, overlay.as_ref());
        layout.push(Layout { objects: r.source.join(".git/objects"), source: r.source, clone: r.path });
    }
    Ok(layout)
}

/// Work from before the pin (§12, `carry`): what the session's overlay holds below the workspace's
/// repository goes into the clone — merged and uncommitted, or a rebase stopped for Resolve —
/// and leaves the overlay. A workspace with unapplied work of its own that conflicts keeps it in
/// the overlay until a later start; the shelf says so. Never keeps the session from starting.
fn carry_into<H: Host>(h: &H, r: &Row, overlay: Option<&Overlay>) {
    let Some(upper) = overlay.and_then(|o| o.upper_of(&r.source)) else { return };
    let started = overlay.map_or(0, |o| o.started);
    let op = key(&format!("carry:{}:{}", r.id, crate::store::new_id()));
    let outcome = carry_steps(h, r, &upper, started);
    // each start looks again: what an earlier one said is superseded
    let _ = h.db(|c| {
        c.execute("UPDATE workspace_journal SET state='done' WHERE workspace_id=?1 AND kind='carry' AND state IN ('waiting','notice','interrupted')", [&r.id])?;
        Ok(())
    });
    let (state, detail) = match outcome {
        Ok(None) => return,
        Ok(Some((state, detail))) => (state, detail),
        Err(e) => ("waiting", format!("Work the agent did in {} before it was pinned could not be brought into the workspace: {e:#}. It stays in the session's overlay and is tried again at the session's next start.", r.source.display())),
    };
    let _ = h.db(|c| {
        intent(c, &op, &r.id, "carry", &json!({"upper": bytes(&upper)}), h.now())?;
        settle(c, &op, state, &detail)
    });
}

/// [`carry_into`]'s work: the journal state and line to record, None when nothing was there.
fn carry_steps<H: Host>(h: &H, r: &Row, upper: &Path, started: i64) -> Result<Option<(&'static str, String)>> {
    let Some(p) = carry::pending(&r.source, &r.path, upper, started)? else { return Ok(None) };
    let src = r.source.display();
    if carry::is_empty(&p) {
        carry::forget(&p)?;
        return Ok(None);
    }
    let how = if p.from_reflog { "" } else { " Its starting point was not in your checkout's history, so the agent's files were taken as they are: look before you apply." };
    let done = if p.from_reflog { "done" } else { "notice" };
    match carry::merge(&r.path, &p)? {
        carry::Merge::Clean { ours, tree } => {
            let n = carry::write_clean(&r.path, &ours, &tree)?;
            carry::forget(&p)?;
            Ok(Some((done, format!("Work the agent did in {src} before it was pinned ({n} files) is in the workspace, uncommitted.{how}"))))
        }
        carry::Merge::Conflict(files) => {
            let git = crate::changes::Git::open(&r.path)?;
            let own = !flow::dirty(&git)?.is_empty() || flow::unapplied(&r.source, &r.path, &r.branch)?;
            drop(git);
            if own {
                return Ok(Some((
                    "waiting",
                    format!(
                        "Work the agent did in {src} before it was pinned conflicts with the workspace's own work ({}). It stays in the session's overlay and is brought in at the session's next start once the workspace is applied or discarded.",
                        files.join(", ")
                    ),
                )));
            }
            let tip = carry::stage_rebase(&r.path, &r.branch, &p)?;
            let out = h.run_jailed(&r.session_id, &r.source, &["git", "rebase", "--onto", &tip, &p.base]);
            match out {
                Ok(o) if o.status.success() => {
                    carry::uncommit(&r.path, &r.branch, &tip)?;
                    carry::forget(&p)?;
                    Ok(Some((done, format!("Work the agent did in {src} before it was pinned is in the workspace, uncommitted.{how}"))))
                }
                Ok(_) if rebasing(&r.path) => {
                    carry::forget(&p)?;
                    let stopped = conflicted(&r.path);
                    let branch = super::source_head(&r.source).map(|(b, _)| b).unwrap_or_default();
                    Ok(Some((
                        "conflict",
                        format!(
                            "Work from before {src} was pinned, carried onto {branch} ({}), stopped: {}",
                            &tip[..tip.len().min(12)],
                            if stopped.is_empty() { files.join(", ") } else { stopped.join(", ") }
                        ),
                    )))
                }
                other => {
                    carry::unstage(&r.path, &r.branch, &tip)?;
                    let why = match other {
                        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                        Err(e) => format!("{e:#}"),
                    };
                    bail!("the rebase could not run: {why}")
                }
            }
        }
    }
}

/// A session's repositories as the window shows them (§12 gap 1): its workspaces, and the
/// repositories it keeps live with why.
pub fn session_view<H: Host>(h: &H, session: &str) -> Result<api::SessionView> {
    let workspaces_mode = h.db(|c| mode(c, session))?;
    let rows = h.db(|c| if has_tables(c)? { of_session(c, session) } else { Ok(vec![]) })?;
    let (mut workspaces, mut details) = (Vec::new(), Vec::new());
    for r in rows {
        match detail(h, r)? {
            Settled::Freed => {}
            Settled::Record(w) => workspaces.push(w),
            Settled::Full(d) => {
                workspaces.push(d.workspace.clone());
                details.push(d);
            }
        }
    }
    let live = h.db(|c| {
        if !has_tables(c)? {
            return Ok(vec![]);
        }
        let mut stmt = c.prepare("SELECT source, reason FROM session_live_repositories WHERE session_id=?1")?;
        let rows = stmt.query_map([session], |r| Ok((path(r.get(0)?), r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let live = live
        .into_iter()
        .map(|(source, reason)| api::LiveRepository {
            message: refusal_message(&reason),
            source: HostPath::from_path(&source),
            reason,
        })
        .collect();
    Ok(api::SessionView { workspaces_mode, workspaces, details, live })
}

fn refusal_message(reason: &str) -> String {
    use super::Refusal::*;
    [NotARepository, NoCommits, LinkedWorktree, Submodules, PartialClone, Lfs, FilterDrivers, BorrowedObjects, DetachedHead, RefFormat]
        .into_iter()
        .find(|r| r.reason() == reason)
        .map(|r| format!("Live: {}", r.message()))
        .unwrap_or_else(|| format!("Live: {reason}"))
}

/// The clone's revision now, against the checkout's current branch, and whether it needs a rebase.
fn revision_now(r: &Row) -> Result<(api::Revision, bool)> {
    let rev = super::revision(&r.path, &r.branch)?;
    let (branch, tip) = super::source_head(&r.source)?;
    let needs_rebase = !super::descends(&r.path, &tip, &rev.tip)?;
    Ok((api::Revision { tip: rev.tip, worktree: rev.worktree, destination_branch: branch, destination_tip: tip }, needs_rebase))
}

/// What an Apply would bring: commits and changes since what the person has (`m`).
fn summary(r: &Row, m: &flow::Mirror) -> Result<api::Summary> {
    let s = super::summary_to(&r.path, &r.branch, &m.upstream, &m.tree, Some(&m.now))?;
    Ok(api::Summary { commits: s.commits, files: s.files, added: s.added.to_string(), removed: s.removed.to_string(), uncommitted: s.uncommitted })
}

/// A workspace after one look (`detail`).
enum Settled {
    /// Its session had ended with nothing left: the folder is gone.
    Freed,
    /// Its folder or checkout is gone: the record alone.
    Record(api::Workspace),
    Full(api::Detail),
}

/// The workspace's record, revision and what an Apply would bring, from one look that also
/// settles an ended session's workspace and a stopped rebase that has since finished.
fn detail<H: Host>(h: &H, r: Row) -> Result<Settled> {
    let l = look(&r)?;
    let r = if ending(h, &r) {
        match settle_ended(h, r, &l)? {
            Some(r) => r,
            None => return Ok(Settled::Freed),
        }
    } else {
        r
    };
    // a stopped rebase the agent or the person finished in the workspace is no longer one: the
    // shelf offers Apply again, not Resolve and Abort for a rebase that is over
    clear_conflict(h, &r)?;
    let workspace = h.db(|c| record(c, &r))?;
    let Some(m) = l else { return Ok(Settled::Record(workspace)) };
    let (revision, needs_rebase) = revision_now(&r)?;
    Ok(Settled::Full(api::Detail { workspace, revision, summary: summary(&r, &m)?, needs_rebase, checkout_only: m.checkout_only.len() as u32 }))
}

fn changed<H: Host>(h: &H, r: Option<&Row>, operation: Option<&str>, discarded: Option<api::Discarded>) -> Result<api::Changed> {
    h.db(|c| {
        Ok(api::Changed {
            workspace: r.map(|r| record(c, r)).transpose()?,
            operation: operation.map(|o| receipts(c)?.get(o)).transpose()?,
            discarded,
        })
    })
}

/// Newest first; `after` is the previous page's `next`. Reading settles ended sessions and expiries.
pub fn list<H: Host>(h: &H, a: &api::List) -> Result<api::Listed> {
    sweep(h)?;
    let limit = a.limit.unwrap_or(64);
    if !(1..=128).contains(&limit) {
        return Err(sill_proto::rpc::Error::invalid_params("limit must be 1–128").into());
    }
    let bad = || -> anyhow::Error { DomainError { code: "invalid_cursor", message: "not a workspaces cursor".into(), details: None }.into() };
    let after = match &a.after {
        None => None,
        Some(cursor) => {
            let (at, id) = cursor.split_once(':').ok_or_else(bad)?;
            Some((at.parse::<i64>().map_err(|_| bad())?, id.to_string()))
        }
    };
    let mut rows = h.db(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM workspaces WHERE (?4 IS NULL OR session_id = ?4) AND (?1 IS NULL OR created_at < ?1 OR (created_at = ?1 AND id < ?2)) ORDER BY created_at DESC, id DESC LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(params![after.as_ref().map(|a| a.0), after.as_ref().map(|a| a.1.clone()).unwrap_or_default(), limit + 1, a.session_id], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let next = (rows.len() > limit as usize).then(|| {
        rows.truncate(limit as usize);
        let last = rows.last().unwrap();
        format!("{}:{}", last.created_at, last.id)
    });
    let (mut workspaces, mut details) = (Vec::new(), Vec::new());
    for r in rows {
        match detail(h, r)? {
            Settled::Freed => {}
            Settled::Record(w) => workspaces.push(w),
            Settled::Full(d) => {
                workspaces.push(d.workspace.clone());
                details.push(d);
            }
        }
    }
    let discarded = if a.after.is_none() {
        h.db(|c| {
            let mut stmt = c.prepare(&format!("SELECT {DISCARD_COLUMNS} FROM workspace_discards WHERE ?1 IS NULL OR session_id=?1 ORDER BY created_at DESC"))?;
            let rows = stmt.query_map([&a.session_id], discard_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.iter().map(discard_record).collect())
        })?
    } else {
        vec![]
    };
    Ok(api::Listed { workspaces, details, discarded, next })
}

pub fn get<H: Host>(h: &H, id: &str) -> Result<api::Detail> {
    match detail(h, get_row(h, id)?)? {
        Settled::Full(d) => Ok(d),
        Settled::Freed => Err(not_found("workspace")),
        Settled::Record(w) => bail!("{}: the workspace's folder or its repository is gone", w.name),
    }
}

pub fn diff<H: Host>(h: &H, a: &api::DiffRequest) -> Result<api::DiffText> {
    let r = get_row(h, &a.workspace_id)?;
    let (against, now) = match a.against {
        api::Against::Applied => {
            let m = flow::mirror(&r.source, &r.path, &r.branch)?;
            (m.tree, m.now)
        }
        api::Against::Base => (r.base.clone(), flow::state_tree(&r.path)?),
    };
    let (text, truncated) = super::diff_to(&r.path, &against, Some(&now), DIFF_LIMIT)?;
    Ok(api::DiffText { text, truncated })
}

/// Rebase the result onto the checkout's current tip when it moved, in the helper jail, keeping
/// uncommitted changes uncommitted (`--autostash`). Repeatable: a retry recomputes.
pub fn prepare<H: Host>(h: &H, a: &api::Operate) -> Result<api::Prepared> {
    const METHOD: &str = "_sill/workspaces.prepare";
    let fingerprint = input(a)?;
    let _g = lock();
    let r = settle_session(h, get_row(h, &a.workspace_id)?)?.ok_or_else(|| not_found("workspace"))?;
    if h.busy(&r.session_id) {
        return Err(refuse("writer_active", "The agent is working; wait for its turn to end.", vec![]));
    }
    clear_conflict(h, &r)?;
    if rebasing(&r.path) {
        return Err(refuse("rebase_in_progress", "A rebase is stopped in the workspace: finish it there or abort it.", conflicted(&r.path)));
    }
    let (branch, _) = super::source_head(&r.source)?;
    if branch.is_empty() {
        return Err(refuse("detached_head", "Check out a branch in your checkout first.", vec![]));
    }
    let (_, needs_rebase) = revision_now(&r)?;
    if needs_rebase && h.life(&r.session_id) == Life::Missing {
        return Err(refuse("no_session", "The workspace's session is gone, so it cannot be rebased.", vec![]));
    }
    let fresh = h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?.1;
    let dest = super::set_destination(&r.source, &r.path, &r.id, &branch, &r.branch)?;
    let mut conflicts = Vec::new();
    if needs_rebase {
        let out = h.run_jailed(&r.session_id, &r.source, &["git", "rebase", "--autostash", "refs/sill/dest"])?;
        if !out.status.success() {
            if rebasing(&r.path) {
                conflicts = conflicted(&r.path);
                let detail = format!(
                    "The rebase onto {branch} ({}) stopped: {}",
                    &dest[..dest.len().min(12)],
                    if conflicts.is_empty() { "see the workspace".into() } else { conflicts.join(", ") }
                );
                let k = key(&a.operation_id);
                h.db(|c| {
                    intent(c, &k, &r.id, "rebase", &json!({"dest": dest}), h.now())?;
                    settle(c, &k, "conflict", &detail)
                })?;
            } else {
                let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let hint = if r.shallow { " The checkout is shallow: fetch more history in it, then try again." } else { "" };
                if fresh {
                    h.db(|c| receipts(c)?.finish(&a.operation_id, None, Some("invalid_state")))?;
                }
                return Err(refuse("rebase_failed", format!("The rebase could not run: {why}.{hint}"), vec![]));
            }
        }
    }
    let (revision, needs_rebase) = revision_now(&r)?;
    if fresh {
        h.db(|c| receipts(c)?.finish(&a.operation_id, None, None))?;
    }
    Ok(api::Prepared { workspace: h.db(|c| record(c, &r))?, revision, needs_rebase, conflicts })
}

/// Abort a rebase that stopped in the workspace, in the same helper jail.
pub fn abort<H: Host>(h: &H, a: &api::Operate) -> Result<api::Changed> {
    let _g = lock();
    let r = get_row(h, &a.workspace_id)?;
    if !rebasing(&r.path) {
        clear_conflict(h, &r)?;
        return changed(h, Some(&r), None, None);
    }
    if h.busy(&r.session_id) {
        return Err(refuse("writer_active", "The agent is working; abort the rebase after its turn.", vec![]));
    }
    let out = h.run_jailed(&r.session_id, &r.source, &["git", "rebase", "--abort"])?;
    if !out.status.success() {
        return Err(refuse("rebase_failed", format!("The rebase could not be aborted: {}", String::from_utf8_lossy(&out.stderr).trim()), vec![]));
    }
    clear_conflict(h, &r)?;
    changed(h, Some(&r), None, None)
}

fn stop_message(stop: &super::Stop) -> String {
    match stop {
        super::Stop::SourceMoved => "Your checkout's branch moved or changed since you looked; look again.".into(),
        super::Stop::NeedsPrepare => "Your checkout has commits the workspace does not: it needs a rebase first.".into(),
        super::Stop::Overlap(p) => format!("Your own edits touch files the agent changed: {}", p.join(", ")),
        super::Stop::Untracked(p) => format!("Untracked files of yours would be overwritten: {}", p.join(", ")),
    }
}

/// Apply (§12): bring the workspace's commits and uncommitted changes into the person's checkout,
/// exactly the revision they saw. The workspace goes on; what it has applied moves forward.
pub fn apply<H: Host>(h: &H, a: &api::Apply) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.apply";
    let fingerprint = input(a)?;
    let k = key(&a.operation_id);
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?.is_some() {
        let r = h.db(|c| load(c, &a.workspace_id))?;
        return changed(h, r.as_ref(), Some(&a.operation_id), None);
    }
    let r = settle_session(h, get_row(h, &a.workspace_id)?)?.ok_or_else(|| not_found("workspace"))?;
    if h.busy(&r.session_id) {
        return Err(refuse("writer_active", "The agent is working; apply after its turn.", vec![]));
    }
    if rebasing(&r.path) {
        return Err(refuse("rebase_in_progress", "A rebase is stopped in the workspace: finish it there or abort it.", conflicted(&r.path)));
    }
    let (now_rev, needs_rebase) = revision_now(&r)?;
    if now_rev != a.revision {
        return Err(refuse("stale_revision", "The workspace or your checkout changed since you looked; look again.", vec![]));
    }
    if needs_rebase {
        return Err(refuse("stale_revision", "Your checkout has commits the workspace does not: prepare (rebase) first.", vec![]));
    }
    if now_rev.destination_branch.is_empty() {
        return Err(refuse("detached_head", "Check out a branch in your checkout first.", vec![]));
    }
    let was = flow::applied(&r.path)?;
    h.db(|c| {
        receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint)?;
        intent(c, &k, &r.id, "apply", &json!({"branch": now_rev.destination_branch, "old": now_rev.destination_tip, "tip": now_rev.tip}), h.now())
    })?;
    let outcome = flow::apply(&r.source, &r.path, &r.id, &now_rev.destination_branch, &now_rev.destination_tip, &r.branch, &was);
    match outcome {
        Ok(Ok(done)) => {
            flow::set_applied(&r.path, &done.snapshot)?;
            h.db(|c| {
                settle(c, &k, "done", "")?;
                settle_notices(c, &r.id)?;
                receipts(c)?.finish(&a.operation_id, None, None)
            })?;
            let r = settle_result(h, r)?;
            changed(h, r.as_ref(), Some(&a.operation_id), None)
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
            let detail = "The Apply stopped part way: check your checkout's branch and the files it changed, then look again.";
            let _ = h.db(|c| {
                settle(c, &k, "interrupted", detail)?;
                receipts(c)?.finish(&a.operation_id, None, Some("operation_failed"))
            });
            Err(e.context(detail))
        }
    }
}

/// Discard (§12): the workspace back to what was last applied; the discarded work kept 7 days for
/// Undo. Only exactly the revision the person saw.
pub fn discard<H: Host>(h: &H, a: &api::Apply) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.discard";
    let fingerprint = input(a)?;
    let _g = lock();
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?
        .is_some()
    {
        let r = h.db(|c| load(c, &a.workspace_id))?;
        return changed(h, r.as_ref(), Some(&a.operation_id), None);
    }
    let mut r =
        settle_session(h, get_row(h, &a.workspace_id)?)?.ok_or_else(|| not_found("workspace"))?;
    if h.busy(&r.session_id) {
        return Err(refuse(
            "writer_active",
            "The agent is working; discard after its turn.",
            vec![],
        ));
    }
    if rebasing(&r.path) {
        return Err(refuse(
            "rebase_in_progress",
            "A rebase is stopped in the workspace: finish it there or abort it first.",
            conflicted(&r.path),
        ));
    }
    let now_rev = super::revision(&r.path, &r.branch)?;
    if now_rev.tip != a.revision.tip || now_rev.worktree != a.revision.worktree {
        return Err(refuse(
            "stale_revision",
            "The workspace changed since you looked; look again at what would be discarded.",
            vec![],
        ));
    }
    h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?;
    // Unique anchors cannot overwrite a snapshot retained by a failed earlier attempt.
    let n = crate::store::new_id();
    let mut discarded = None;
    flow::discard_retained(
        &r.source.clone(),
        &r.path.clone(),
        &r.id.clone(),
        &n,
        &r.branch.clone(),
        |kept| {
            let d = Discard {
                id: format!("wsd-{}", crate::store::new_id()),
                workspace_id: r.id.clone(),
                session_id: r.session_id.clone(),
                name: r.name.clone(),
                source: r.source.clone(),
                branch: r.branch.clone(),
                snapshot: kept.to_string(),
                anchor: super::anchor_ref(&r.id, &format!("discarded/{n}")),
                until: h.now() + RETENTION,
                created_at: h.now(),
            };
            r.discards += 1;
            r.updated_at = h.now();
            h.db(|c| {
        let tx = c.unchecked_transaction()?;
        save(c, &r)?;
        c.execute(
            &format!("INSERT INTO workspace_discards({DISCARD_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
            params![d.id, d.workspace_id, d.session_id, d.name, bytes(&d.source), d.branch, d.snapshot, d.anchor, d.until, d.created_at],
        )?;
        settle_notices(c, &d.workspace_id)?;
        intent(c, &key(&a.operation_id), &r.id, "discard", &json!({"snapshot": kept, "anchor": d.anchor}), h.now())?;
        tx.commit()?;
        Ok(())
    })?;
            discarded = Some(d);
            Ok(())
        },
    )?;
    h.db(|c| {
        settle(c, &key(&a.operation_id), "done", "")?;
        receipts(c)?.finish(&a.operation_id, None, None)
    })?;
    let d = discarded.context("discard snapshot was not retained")?;
    let r = settle_result(h, r)?;
    changed(
        h,
        r.as_ref(),
        Some(&a.operation_id),
        Some(discard_record(&d)),
    )
}

/// Undo a Discard: the work back into its workspace when nothing happened there since, or into a
/// workspace of its own when its session's folder is gone (§12).
pub fn undo<H: Host>(h: &H, a: &api::Undo) -> Result<api::Changed> {
    const METHOD: &str = "_sill/workspaces.undo";
    let fingerprint = input(a)?;
    let _g = lock();
    let d = h
        .db(|c| {
            Ok(c.query_row(
                &format!("SELECT {DISCARD_COLUMNS} FROM workspace_discards WHERE id=?1"),
                [&a.discard_id],
                discard_row,
            )
            .optional()?)
        })?
        .ok_or_else(|| not_found("discarded work"))?;
    if h.db(|c| receipts(c)?.existing(&a.operation_id, METHOD, &fingerprint))?
        .is_some()
    {
        let r = h.db(|c| load(c, &d.workspace_id))?;
        return changed(h, r.as_ref(), Some(&a.operation_id), None);
    }
    if h.now() > d.until {
        return Err(refuse(
            "retention_expired",
            "The discarded work is no longer kept.",
            vec![],
        ));
    }
    let existing = h
        .db(|c| load(c, &d.workspace_id))?
        .filter(|r| r.path.exists());
    let r = match existing {
        Some(r) => {
            if h.busy(&r.session_id) {
                return Err(refuse(
                    "writer_active",
                    "The agent is working; undo after its turn.",
                    vec![],
                ));
            }
            h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?;
            if !flow::undo(&r.source, &r.path, &r.branch, &d.snapshot)? {
                let _ = h.db(|c| receipts(c)?.finish(&a.operation_id, None, Some("invalid_state")));
                return Err(refuse(
                    "changed_since",
                    "The workspace has new work since the Discard; apply or discard it first.",
                    vec![],
                ));
            }
            r
        }
        None => {
            if h.busy(&d.session_id) {
                return Err(refuse(
                    "writer_active",
                    "The agent is working; undo after its turn.",
                    vec![],
                ));
            }
            if h.db(|c| of_session(c, &d.session_id))?
                .iter()
                .any(|r| r.source == d.source)
            {
                return Err(refuse(
                    "changed_since",
                    "This session already has a workspace for this repository; its newer work must be settled before Undo.",
                    vec![],
                ));
            }
            // its folder went: a workspace of its own, a result for the person to decide on
            let src = match super::inspect(&d.source)? {
                Ok(s) => s,
                Err(refusal) => return Err(refuse(refusal.reason(), refusal.message(), vec![])),
            };
            h.db(|c| receipts(c)?.begin(&a.operation_id, METHOD, &fingerprint))?;
            let id = format!("ws-{}", crate::store::new_id());
            let at = super::path_for(&h.root(), &src, &id, &d.name);
            let op = key(&a.operation_id);
            let made = super::create(&src, &id, &d.name, &at, &op)?;
            flow::reset_to_snapshot(&made.path, &made.branch, &d.snapshot)?;
            let now = h.now();
            let r = Row {
                id,
                name: d.name.clone(),
                session_id: d.session_id.clone(),
                source: src.root.clone(),
                source_branch: src.branch.clone(),
                base: made.base,
                branch: made.branch,
                path: made.path,
                state: "result".into(),
                shallow: src.shallow,
                discards: 0,
                created_by: op,
                created_at: now,
                updated_at: now,
            };
            h.db(|c| save(c, &r))?;
            r
        }
    };
    if d.source.exists() {
        let _ = drop_ref(&d.source, &d.anchor);
    }
    h.db(|c| {
        c.execute("DELETE FROM workspace_discards WHERE id=?1", [&d.id])?;
        receipts(c)?.finish(&a.operation_id, None, None)
    })?;
    changed(h, Some(&r), Some(&a.operation_id), None)
}

/// What the agent of a workspace session is told (§12): its pinned repositories are its own copies
/// at their usual paths; the person applies or discards; it does not push.
pub fn session_notice(conn: &Connection, session: &str) -> Result<Option<String>> {
    if !has_tables(conn)? || !mode(conn, session)? {
        return Ok(None);
    }
    let rows = of_session(conn, session)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let repos = rows.iter().map(|r| format!("{} (branch {})", r.source.display(), r.branch)).collect::<Vec<_>>().join(", ");
    let mut notice = format!(
        "\n[This session works in workspaces: {repos} — each is your own copy of the repository at its usual path; the person's checkout is elsewhere and changes only when they apply your work. Commit or leave changes uncommitted as suits the work; the person applies both, or discards them. Do not push.]"
    );
    // the person's uncommitted work is not in the copy (§5): said, so the agent does not look
    // for code that is only in their checkout. A look that fails says nothing.
    for r in rows.iter().filter(|r| r.state == "open") {
        let Ok(Some(m)) = look(r) else { continue };
        if m.checkout_only.is_empty() {
            continue;
        }
        const NAMED: usize = 8;
        let n = m.checkout_only.len();
        let mut names = m.checkout_only.iter().take(NAMED).cloned().collect::<Vec<_>>().join(", ");
        if n > NAMED {
            names.push_str(&format!(" and {} more", n - NAMED));
        }
        notice.push_str(&format!(
            "\n[The person's checkout of {} has {n} uncommitted file{} of their own that your copy does not have ({names}): your copy started from their last commit. If they refer to work you cannot find, it may be there — say so rather than guess.]",
            r.source.display(),
            if n == 1 { "" } else { "s" },
        ));
    }
    Ok(Some(notice))
}

/// The end of retention: discarded work past its time goes, with its anchor.
pub fn sweep<H: Host>(h: &H) -> Result<Vec<String>> {
    let now = h.now();
    let due = h.db(|c| {
        if !has_tables(c)? {
            return Ok(vec![]);
        }
        let mut stmt = c.prepare(&format!("SELECT {DISCARD_COLUMNS} FROM workspace_discards WHERE until < ?1"))?;
        let rows = stmt.query_map([now], discard_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let mut gone = Vec::new();
    for d in due {
        if d.source.exists() {
            let _ = drop_ref(&d.source, &d.anchor);
        }
        h.db(|c| {
            c.execute("DELETE FROM workspace_discards WHERE id=?1", [&d.id])?;
            Ok(())
        })?;
        gone.push(d.id);
    }
    Ok(gone)
}

/// At daemon start, before serving: settle unfinished journal entries from what is on disk, then
/// settle ended sessions and expire what is due.
pub fn reconcile<H: Host>(h: &H) -> Result<()> {
    let guard = lock();
    let running: Vec<(String, String, String, String)> = h.db(|c| {
        let mut stmt = c.prepare("SELECT operation,workspace_id,kind,intent FROM workspace_journal WHERE state='running'")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    for (op, id, kind, intent_text) in running {
        let i: Value = serde_json::from_str(&intent_text).unwrap_or(Value::Null);
        let p = |name: &str| -> PathBuf { path(i[name].as_array().map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default()) };
        match kind.as_str() {
            "create" => {
                if h.db(|c| load(c, &id))?.is_some() {
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
            _ => h.db(|c| settle(c, &op, "interrupted", "Interrupted by a restart: check your checkout before going on."))?,
        }
    }
    drop(guard);
    let rows = h.db(|c| {
        let mut stmt = c.prepare(&format!("SELECT {COLUMNS} FROM workspaces"))?;
        let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    for r in rows {
        settle_session(h, r)?;
    }
    // spare folders of before 2026-11-09, when a finished folder was kept for the next session
    let spares = h.root().join(".spare");
    if spares.exists() {
        let _ = super::remove_folder(&spares);
    }
    sweep(h)?;
    Ok(())
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
