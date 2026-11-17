//! Crosscheck (docs/crosscheck.md): a second opinion on the work waiting in a workspace. One
//! Reviewer, a worker session of its own on a disposable copy of the captured work, reports a
//! short summary and a list of problems. The review is recorded before its worker starts, and
//! nothing is ever started again by itself: a restart ends a review under way as failed.
use super::*;
use anyhow::ensure;
use rusqlite::OptionalExtension;
use sill_core::{
    crosscheck as capture,
    sfp::Journal,
    workspace::{flow, records},
};
use sill_proto::method;
use sill_proto::{crosscheck as api, sfp as p};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a worker may take to report its controls and take the Reviewer's options.
const CONTROLS_WAIT: Duration = Duration::from_secs(45);
/// What the Reviewer is told of what the person asked for, at most.
const REQUEST_BYTES: usize = 16_384;

pub(super) fn init(db: &rusqlite::Connection) -> Result<()> {
    // `crosscheck_workers` is as the first build (2026-11-13) made it: its worker sessions stay
    // hidden. That build's `crosscheck_runs` and `crosscheck_reviewers` are left as they are.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS crosscheck_reviews (
            id TEXT PRIMARY KEY, operation_id TEXT UNIQUE NOT NULL, workspace_id TEXT NOT NULL, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS crosscheck_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS crosscheck_workers (session_id TEXT PRIMARY KEY, run_id TEXT NOT NULL);",
    )?;
    Ok(())
}
fn active(state: &str) -> bool {
    matches!(state, "capturing" | "reviewing")
}
fn root(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".local/share")).join("sill/crosschecks")
}
fn parse<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::invalid_params(e.to_string()).into())
}
fn gone() -> anyhow::Error {
    Error::domain("not_found", "This review was removed").into()
}
fn load(db: &rusqlite::Connection, id: &str) -> Result<api::Run> {
    let record: Option<String> = db.query_row("SELECT record FROM crosscheck_reviews WHERE id=?1", [id], |r| r.get(0)).optional()?;
    Ok(serde_json::from_str(&record.ok_or_else(gone)?)?)
}
fn save(db: &rusqlite::Connection, run: &api::Run) -> Result<()> {
    db.execute("UPDATE crosscheck_reviews SET record=?2 WHERE id=?1", rusqlite::params![run.run_id, serde_json::to_string(run)?])?;
    Ok(())
}
fn all(db: &rusqlite::Connection) -> Result<Vec<api::Run>> {
    let mut q = db.prepare("SELECT record FROM crosscheck_reviews")?;
    let records = q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(records.iter().filter_map(|r| serde_json::from_str(r).ok()).collect())
}
fn validate_reviewers(reviewers: &[api::Reviewer]) -> Result<()> {
    let text = |s: &str, most: usize| s.len() <= most && !s.chars().any(char::is_control);
    ensure!(reviewers.len() <= 16, "at most sixteen Reviewers");
    for r in reviewers {
        ensure!(!r.agent.is_empty() && text(&r.agent, 128), "invalid Reviewer agent");
        ensure!(text(&r.name, 128), "invalid Reviewer name");
        ensure!(r.options.len() <= 64 && r.options.iter().all(|(k, v)| !k.is_empty() && text(k, 256) && text(v, 256)), "invalid Reviewer options");
    }
    Ok(())
}

impl Hub {
    pub(in crate::daemon) fn crosscheck_worker(&self, id: &str) -> bool {
        self.with_store(|s| Ok(s.conn().query_row("SELECT EXISTS(SELECT 1 FROM crosscheck_workers WHERE session_id=?1)", [id], |r| r.get::<_, bool>(0))?))
            .unwrap_or(true)
    }
    fn cc_update(&self, id: &str, f: impl FnOnce(&mut api::Run)) -> Result<api::Run> {
        let run = {
            let _lock = self.crosscheck_lock.lock().unwrap();
            self.with_store(|s| {
                let mut run = load(s.conn(), id)?;
                f(&mut run);
                save(s.conn(), &run)?;
                Ok(run)
            })?
        };
        self.cc_notice(&run.workspace_id, &run.run_id);
        Ok(run)
    }
    fn cc_notice(&self, workspace: &str, run: &str) {
        let req = sill_proto::rpc::Request::new(None, method::CROSSCHECK_CHANGED, json!(api::Notice { workspace_id: workspace.into(), run_id: run.into() }));
        if let Ok(line) = serde_json::to_string(&req) {
            for conn in self.connections().into_iter().filter(|c| c.has_capability(api::CAPABILITY)) {
                conn.send(line.clone());
            }
        }
    }
    /// Whether the review is still wanted: the person may remove it at any moment.
    fn cc_wanted(&self, id: &str) -> Result<()> {
        ensure!(active(&self.with_store(|s| load(s.conn(), id))?.state), "The review was stopped");
        Ok(())
    }
    /// A workspace's review with whether it still describes the workspace's files.
    fn cc_current(&self, workspace: &str) -> Result<api::Current> {
        let id: Option<String> = self.with_store(|s| {
            Ok(s.conn()
                .query_row("SELECT id FROM crosscheck_reviews WHERE workspace_id=?1 ORDER BY rowid DESC LIMIT 1", [workspace], |r| r.get(0))
                .optional()?)
        })?;
        let Some(id) = id else { return Ok(api::Current { run: None }) };
        let mut run = self.with_store(|s| load(s.conn(), &id))?;
        run.stale = self.workspace_detail_for_crosscheck(workspace).map_or(true, |d| d.state_tree != run.state_tree);
        Ok(api::Current { run: Some(run) })
    }

    /// The saved Reviewers and what each agent offers. An agent's options are known only from a
    /// session of it: the latest one that reported any is read.
    fn cc_reviewers(&self) -> Result<api::Reviewers> {
        let registry = sill_core::agents::load(&self.home)?;
        self.with_store(|s| {
            let saved: Option<String> = s.conn().query_row("SELECT value FROM crosscheck_settings WHERE key='reviewers'", [], |r| r.get(0)).optional()?;
            let reviewers = saved.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
            let journal = Journal::new(s)?;
            let sessions = s.sessions()?;
            let agents = registry
                .values()
                .filter(|a| a.enabled && a.ready(&self.home).is_ok())
                .map(|a| {
                    let options = sessions
                        .iter()
                        .filter(|x| x.agent == a.id)
                        .take(64)
                        .filter_map(|x| journal.session(&x.id).ok()?.controls)
                        .find(|c| c.status == "ready" && !c.options.is_empty())
                        .map(|c| c.options)
                        .unwrap_or_default();
                    api::AgentOptions { agent: a.id.clone(), title: a.name.clone(), options }
                })
                .collect();
            Ok(api::Reviewers { reviewers, agents })
        })
    }
    /// Who reviews the work of a session of `author`: the first saved Reviewer. When none was
    /// chosen, the agent of the person's latest session that is not the author's: one they use,
    /// so one that is signed in. With no such session the author's own agent reviews.
    fn cc_reviewer(&self, author: &str) -> Result<api::Reviewer> {
        let setup = self.cc_reviewers()?;
        let offered = |agent: &str| setup.agents.iter().any(|a| a.agent == agent);
        if let Some(saved) = setup.reviewers.first() {
            ensure!(offered(&saved.agent), "The Reviewer's agent, {}, is not set up here: choose a Reviewer in Settings", saved.agent);
            return Ok(saved.clone());
        }
        let used = self.with_store(|s| {
            let mut q = s
                .conn()
                .prepare("SELECT agent FROM sessions WHERE ghost=0 AND id NOT IN (SELECT session_id FROM crosscheck_workers) ORDER BY last_used DESC")?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        })?;
        let agent = used.iter().find(|a| *a != author && offered(a)).map(String::as_str).unwrap_or(author);
        ensure!(offered(agent), "No agent is set up to review: choose a Reviewer in Settings");
        Ok(api::Reviewer { name: String::new(), agent: agent.into(), options: Default::default() })
    }

    pub(super) fn crosscheck(self: &Arc<Self>, conn: &Conn, method: &str, params: Value) -> Option<Result<Value>> {
        if !method.starts_with("_sill/crosscheck.") {
            return None;
        }
        if !conn.has_capability(api::CAPABILITY) {
            let mut error = Error::domain("unsupported", "Crosscheck needs the sill.crosscheck capability");
            error.data.as_mut().unwrap()["details"] = json!({"capability": api::CAPABILITY});
            return Some(Err(error.into()));
        }
        Some((|| match method {
            method::CROSSCHECK_REVIEWERS => {
                parse::<api::Empty>(params)?;
                Ok(json!(self.cc_reviewers()?))
            }
            method::CROSSCHECK_SET_REVIEWERS => {
                let a: api::SetReviewers = parse(params)?;
                validate_reviewers(&a.reviewers)?;
                self.with_store(|s| {
                    s.conn().execute(
                        "INSERT INTO crosscheck_settings(key,value) VALUES('reviewers',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        [serde_json::to_string(&a.reviewers)?],
                    )?;
                    Ok(())
                })?;
                Ok(json!(self.cc_reviewers()?))
            }
            method::CROSSCHECK_GET => Ok(json!(self.cc_current(&parse::<api::WorkspaceKey>(params)?.workspace_id)?)),
            method::CROSSCHECK_START => Ok(json!(self.cc_start(parse(params)?)?)),
            method::CROSSCHECK_REMOVE => {
                self.cc_remove(&parse::<api::RunKey>(params)?.run_id)?;
                Ok(json!({}))
            }
            _ => Err(Error::method_not_found(method).into()),
        })())
    }

    fn cc_start(self: &Arc<Self>, a: api::Start) -> Result<api::Run> {
        ensure!(!a.operation_id.is_empty() && a.operation_id.len() <= 128 && !a.operation_id.chars().any(char::is_control), "invalid operation_id");
        let earlier = {
            let _lock = self.crosscheck_lock.lock().unwrap();
            if let Some(id) = self.with_store(|s| {
                Ok(s.conn().query_row("SELECT id FROM crosscheck_reviews WHERE operation_id=?1", [&a.operation_id], |r| r.get::<_, String>(0)).optional()?)
            })? {
                return self.with_store(|s| load(s.conn(), &id));
            }
            let d = self.workspace_detail_for_crosscheck(&a.workspace_id)?;
            ensure!(
                d.workspace.operation.as_ref().is_none_or(|o| matches!(o.state.as_str(), "notice" | "waiting")),
                "Finish what the workspace is doing before a Crosscheck"
            );
            let (source, path) = (d.workspace.source.to_path()?, d.workspace.path.to_path()?);
            // what an Apply would bring, as the shelf counts it: nothing else is reviewed
            let mirror = flow::mirror(&source, &path, &d.workspace.branch)?;
            ensure!(mirror.changed && !d.state_tree.is_empty(), "There is nothing to review: the workspace has no work waiting");
            // A copy of the clone must not show a Reviewer what the producing jail hid.
            let plan = sill_core::session::recorded_filesystem(&d.workspace.session_id)
                .context("Crosscheck needs the session's recorded filesystem: start the session once, then review")?;
            let masks = plan.policy_paths.masks.iter().chain(&plan.agent_masks).map(|m| m.path.to_path()).collect::<Result<Vec<_>>>()?;
            ensure!(
                !masks_intersect(&source, &masks),
                "This repository has paths hidden from its agent, which a Reviewer's copy could show: it cannot be reviewed yet"
            );
            let author = self.with_store(|s| s.session_exact(&d.workspace.session_id))?.map(|x| x.agent).unwrap_or_default();
            let reviewer = self.cc_reviewer(&author)?;
            let run = api::Run {
                run_id: sill_core::store::new_id(),
                workspace_id: a.workspace_id.clone(),
                session_id: d.workspace.session_id.clone(),
                state_tree: d.state_tree.clone(),
                base: mirror.upstream,
                state: "capturing".into(),
                detail: String::new(),
                reviewers: vec![api::Participant {
                    reviewer,
                    session_id: None,
                    state: "starting".into(),
                    summary: String::new(),
                    activity: vec![],
                    error: None,
                }],
                problems: vec![],
                created_at: sill_core::store::now().to_string(),
                stale: false,
            };
            let earlier = self.with_store(|s| {
                // one review a workspace: a finished one is replaced, one under way is not
                let earlier: Vec<api::Run> = all(s.conn())?.into_iter().filter(|r| r.workspace_id == run.workspace_id).collect();
                ensure!(!earlier.iter().any(|r| active(&r.state)), "A review of this work is already under way");
                s.conn().execute("DELETE FROM crosscheck_reviews WHERE workspace_id=?1", [&run.workspace_id])?;
                s.conn().execute(
                    "INSERT INTO crosscheck_reviews(id,operation_id,workspace_id,record) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![run.run_id, a.operation_id, run.workspace_id, serde_json::to_string(&run)?],
                )?;
                Ok(earlier)
            })?;
            let (hub, id) = (self.clone(), run.run_id.clone());
            if let Err(e) = std::thread::Builder::new().name(format!("sill-crosscheck-{id}")).spawn(move || hub.cc_run(&id, &path)) {
                self.with_store(|s| {
                    let mut r = load(s.conn(), &run.run_id)?;
                    fail(&mut r, &e.into());
                    save(s.conn(), &r)
                })?;
            }
            self.cc_notice(&run.workspace_id, &run.run_id);
            (earlier, run)
        };
        for old in earlier.0 {
            let _ = std::fs::remove_dir_all(root(&self.home).join(&old.run_id));
        }
        Ok(earlier.1)
    }

    /// The review's thread, start to end. Whatever happens, its worker is stopped and its files
    /// are gone when this returns: a review keeps its words, not its copies.
    fn cc_run(self: &Arc<Self>, id: &str, workspace: &Path) {
        let dir = root(&self.home).join(id);
        if let Err(e) = self.cc_execute(id, workspace, &dir) {
            // a removed review has nobody to tell
            let _ = self.cc_update(id, |r| {
                if active(&r.state) {
                    fail(r, &e)
                }
            });
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    fn cc_execute(self: &Arc<Self>, id: &str, workspace: &Path, dir: &Path) -> Result<()> {
        let run = self.with_store(|s| load(s.conn(), id))?;
        let deadline = Instant::now() + Duration::from_secs(api::TIME_LIMIT_SECS);
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let snapshot = capture::capture(workspace, &run.base, &dir.join("snapshot"))?;
        ensure!(snapshot.tree == run.state_tree, "The work changed while it was being captured: press Crosscheck again");
        let copy = capture::reviewer_copy(&snapshot, &dir.join("review-0"))?;
        self.cc_update(id, |r| {
            if active(&r.state) {
                r.state = "reviewing".into();
            }
        })?;
        let reviewer = run.reviewers[0].reviewer.clone();
        let session = self.cc_worker(id, &run.session_id, &reviewer, &copy)?;
        let answer = (|| {
            self.cc_update(id, |r| {
                r.reviewers[0].session_id = Some(session.clone());
                r.reviewers[0].state = "running".into();
            })?;
            let asked = self.with_store(|s| requests(s, &run.session_id))?;
            self.cc_prompt(id, &session, &reviewer, &prompt(&run, &copy, &asked), deadline)
        })();
        // The worker is stopped whatever it answered, and before its answer is read.
        if let Err(e) = self.cc_finish_worker(&session) {
            eprintln!("sill Crosscheck: stopping worker {session}: {e:#}");
        }
        let report = decode_report(&answer?)?;
        self.cc_update(id, |r| {
            if active(&r.state) {
                r.state = "complete".into();
                r.reviewers[0].state = "complete".into();
                r.reviewers[0].summary = report.summary;
                r.problems = report.problems;
            }
        })?;
        Ok(())
    }
    fn cc_worker(&self, run: &str, parent: &str, reviewer: &api::Reviewer, copy: &Path) -> Result<String> {
        let source = self.with_store(|s| s.session_exact(parent))?.context("The session whose work this is no longer exists")?;
        let mut policy = sill_core::session::policy_of(&source, &self.home)?;
        policy.ghostly(&self.home);
        policy.processes.expose.clear();
        policy.network.allow_additional_hosts = false;
        policy.limits.agent_idle_minutes = 0;
        // The policy file is the daemon's, out of every jail's reach. The one pin is the copy.
        let dir = sill_core::store::default_path()?.parent().context("state dir")?.join("crosscheck-policies");
        std::fs::create_dir_all(&dir)?;
        let policy_file = dir.join(format!("{}.toml", sill_core::store::new_id()));
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&policy_file)?;
        file.write_all(toml::to_string(&policy)?.as_bytes())?;
        let made = self.with_store(|s| {
            let tx = s.conn().unchecked_transaction()?;
            // a review removed meanwhile gets no worker
            ensure!(active(&load(s.conn(), run)?.state), "The review was stopped");
            let session = Self::create_session_row(
                s,
                NewSession {
                    agent: reviewer.agent.clone(),
                    policy: policy_file.to_string_lossy().into_owned(),
                    pins: vec![copy.into()],
                    memory_scope: "off".into(),
                    follows: String::new(),
                    remember_pins: false,
                    ghost: false,
                },
            )?;
            records::set_mode(s.conn(), &session, false)?;
            s.set_auto_allow(&session, true)?;
            // the Reviewer's own options, not the person's defaults for this agent
            s.set_session_settings(&session, &sill_core::store::AgentSettings { mode: None, config: reviewer.options.clone() })?;
            s.conn().execute("INSERT INTO crosscheck_workers(session_id,run_id) VALUES(?1,?2)", rusqlite::params![session, run])?;
            tx.commit()?;
            Ok(session)
        });
        if made.is_err() {
            let _ = std::fs::remove_file(&policy_file);
        }
        made
    }
    /// Ask the worker and wait for its whole answer, keeping the review's steps current.
    fn cc_prompt(self: &Arc<Self>, run: &str, session: &str, reviewer: &api::Reviewer, prompt: &str, deadline: Instant) -> Result<String> {
        self.cc_wanted(run)?;
        let runner = self.ensure_runner(session, false)?;
        self.cc_options(run, session, &runner, reviewer, deadline)?;
        self.cc_wanted(run)?;
        let submission = sill_core::store::new_id();
        runner.sfp_submit(
            self,
            p::Submit { session_id: session.into(), submission_id: submission.clone(), text: prompt.into(), attachments: vec![], purpose: None },
        )?;
        let mut shown = Vec::new();
        loop {
            let (s, records, state) = self.with_store(|store| {
                let journal = Journal::new(store)?;
                Ok((journal.submission(session, &submission)?, journal.snapshot(session)?.1, journal.session(session)?))
            })?;
            let steps = activity(&records);
            if steps != shown {
                shown = steps.clone();
                self.cc_update(run, |r| r.reviewers[0].activity = steps)?;
            }
            if s.outcome != "pending" {
                ensure!(s.outcome == "completed", "The Reviewer's turn ended without an answer ({})", s.outcome);
                let said = answer(&records);
                ensure!(!said.trim().is_empty(), "The Reviewer answered nothing");
                return Ok(said);
            }
            self.cc_wanted(run)?;
            ensure!(Instant::now() < deadline, "Stopped after {} minutes", api::TIME_LIMIT_SECS / 60);
            ensure!(state.authentication.is_none(), "{} needs signing in: sign in on the Agents page, then review again", reviewer.agent);
            ensure!(state.question_ids.is_empty(), "The Reviewer asked a question instead of reviewing");
            ensure!(runner.alive(), "The connection to the Reviewer ended");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    /// Wait until the worker has taken the Reviewer's options. They were saved as the session's
    /// own, which the runner restores as it does for any session; a review does not start on
    /// options the person did not choose.
    fn cc_options(&self, run: &str, session: &str, runner: &Arc<Runner>, reviewer: &api::Reviewer, deadline: Instant) -> Result<()> {
        let limit = deadline.min(Instant::now() + CONTROLS_WAIT);
        loop {
            self.cc_wanted(run)?;
            if let Ok(state) = self.with_store(|s| Journal::new(s)?.session(session)) {
                ensure!(state.authentication.is_none(), "{} needs signing in: sign in on the Agents page, then review again", reviewer.agent);
                match state.controls {
                    Some(c) if c.status == "failed" => bail!("The Reviewer's options could not be read: {}", c.error.as_deref().unwrap_or("unknown error")),
                    Some(c) if c.status == "ready" => {
                        let taken = |id: &str, want: &str| {
                            c.options.iter().find(|o| o.option_id == id).is_some_and(|o| match &o.value {
                                p::ControlValue::Id { value } => value == want,
                                p::ControlValue::Boolean { value } => value.to_string() == want,
                            })
                        };
                        if reviewer.options.iter().all(|(id, want)| taken(id, want)) {
                            return Ok(());
                        }
                        let missing: Vec<&str> =
                            reviewer.options.keys().filter(|id| !c.options.iter().any(|o| &o.option_id == *id)).map(String::as_str).collect();
                        ensure!(
                            missing.is_empty() && c.error.is_none(),
                            "{} no longer offers what the Reviewer is set to ({}): choose its options again in Settings",
                            reviewer.agent,
                            c.error.clone().unwrap_or_else(|| missing.join(", "))
                        );
                    }
                    Some(_) => {}
                    // some agents report no controls at all: they review as they start
                    None if reviewer.options.is_empty() && runner.agent_idle() => return Ok(()),
                    None => {}
                }
            }
            ensure!(Instant::now() < limit, "{} did not take the Reviewer's options in time", reviewer.agent);
            ensure!(runner.alive(), "The connection to the Reviewer ended before it started");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    fn cc_finish_worker(&self, id: &str) -> Result<()> {
        self.stop_session(id)?;
        let pins = self.with_store(|s| {
            s.conn().execute("UPDATE sessions SET ended_at=COALESCE(ended_at,?2) WHERE id=?1", rusqlite::params![id, sill_core::store::now()])?;
            s.pins(id)
        })?;
        // Only copies under the crosschecks directory are the worker's to lose.
        if let Ok(owned) = root(&self.home).canonicalize() {
            for pin in pins {
                if pin.canonicalize().is_ok_and(|p| p.starts_with(&owned) && p != owned) {
                    let _ = std::fs::remove_dir_all(pin);
                }
            }
        }
        let _ = sill_core::session::remove_state(rustix::process::getuid().as_raw(), id);
        // its policy file was written for this session alone
        if let (Ok(Some(session)), Ok(db)) = (self.with_store(|s| s.session_exact(id)), sill_core::store::default_path()) {
            let policy = PathBuf::from(&session.policy);
            if db.parent().is_some_and(|state| policy.parent() == Some(&state.join("crosscheck-policies"))) {
                let _ = std::fs::remove_file(policy);
            }
        }
        Ok(())
    }
    fn cc_stop_workers(&self, run: &str) {
        let ids = self
            .with_store(|s| {
                let mut q = s
                    .conn()
                    .prepare("SELECT w.session_id FROM crosscheck_workers w JOIN sessions s ON s.id=w.session_id WHERE w.run_id=?1 AND s.ended_at IS NULL")?;
                Ok(q.query_map([run], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap_or_default();
        for id in ids {
            if let Err(e) = self.cc_finish_worker(&id) {
                eprintln!("sill Crosscheck: stopping worker {id}: {e:#}");
            }
        }
    }
    /// The cross: a review under way is cancelled, a finished one forgotten. The review's own
    /// thread sees its record gone and stops its worker; a worker it had already made is stopped
    /// here too, so the cancel does not wait for the thread's next look.
    fn cc_remove(self: &Arc<Self>, id: &str) -> Result<()> {
        let workspace = {
            let _lock = self.crosscheck_lock.lock().unwrap();
            self.with_store(|s| {
                let workspace: Option<String> = s.conn().query_row("SELECT workspace_id FROM crosscheck_reviews WHERE id=?1", [id], |r| r.get(0)).optional()?;
                s.conn().execute("DELETE FROM crosscheck_reviews WHERE id=?1", [id])?;
                Ok(workspace)
            })?
        };
        let Some(workspace) = workspace else { return Ok(()) };
        self.cc_notice(&workspace, id);
        let (hub, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || {
            hub.cc_stop_workers(&id);
            let _ = std::fs::remove_dir_all(root(&hub.home).join(&id));
        });
        Ok(())
    }
    /// The workspace's work was applied or discarded: its review described work that waited and
    /// goes with it, and a Reviewer still at work on it is stopped as by the cross. The line it
    /// would be cancelled from is gone from the shelf with the work.
    pub(super) fn crosscheck_work_gone(self: &Arc<Self>, workspace: &str) {
        let ids = self.with_store(|s| {
            let mut q = s.conn().prepare("SELECT id FROM crosscheck_reviews WHERE workspace_id=?1")?;
            Ok(q.query_map([workspace], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        });
        match ids {
            Ok(ids) => {
                for id in ids {
                    if let Err(e) = self.cc_remove(&id) {
                        eprintln!("sill Crosscheck: removing review {id} of work no longer waiting: {e:#}");
                    }
                }
            }
            Err(e) => eprintln!("sill Crosscheck: reading the reviews of workspace {workspace}: {e:#}"),
        }
    }
    /// At start: a review that was under way is failed, never resumed, and no worker is left.
    pub(super) fn crosscheck_recover(&self) {
        for run in self.with_store(|s| all(s.conn())).unwrap_or_default().into_iter().filter(|r| active(&r.state)) {
            let _ = self.cc_update(&run.run_id, |r| fail(r, &anyhow::anyhow!("Sill restarted during the review")));
            let _ = std::fs::remove_dir_all(root(&self.home).join(&run.run_id));
        }
        let ids = self
            .with_store(|s| {
                let mut q = s.conn().prepare("SELECT w.session_id FROM crosscheck_workers w JOIN sessions s ON s.id=w.session_id WHERE s.ended_at IS NULL")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap_or_default();
        for id in ids {
            if let Err(e) = self.cc_finish_worker(&id) {
                eprintln!("sill Crosscheck: recovering worker {id}: {e:#}");
            }
        }
    }
}

/// The review did not finish: `why` in a few words for the line, whole in the Reviewer's error.
fn fail(run: &mut api::Run, why: &anyhow::Error) {
    run.state = "failed".into();
    run.detail = why.to_string();
    if let Some(p) = run.reviewers.first_mut() {
        p.state = "failed".into();
        p.error = Some(format!("{why:#}"));
    }
}
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[…]", &text[..end])
}
/// What the person asked for in the session, oldest first: their own messages, not the prompts
/// sill wrote for them.
fn requests(store: &Store, session: &str) -> Result<String> {
    // a session that never ran has no journal, and asked for nothing
    let records = Journal::new(store)?.snapshot(session).map(|s| s.1).unwrap_or_default();
    let asked: Vec<String> = records
        .into_iter()
        .filter_map(|r| match r {
            p::Record::Submission { submission } if submission.purpose.is_none() => Some(submission.text),
            _ => None,
        })
        .collect();
    Ok(bounded(&asked.join("\n\n"), REQUEST_BYTES))
}
/// The agent's last message: its report.
fn answer(records: &[p::Record]) -> String {
    records
        .iter()
        .rev()
        .find_map(|r| match r {
            p::Record::Item { item } if item.kind == "agent" => {
                Some(item.content.iter().filter_map(|c| c.text.as_deref()).collect::<Vec<_>>().join("\n")).filter(|t| !t.trim().is_empty())
            }
            _ => None,
        })
        .unwrap_or_default()
}
/// The worker's latest steps, one line each: what a frontend shows as its progress. The prompt it
/// was given and its report are not steps.
fn activity(records: &[p::Record]) -> Vec<String> {
    const KEPT: usize = 40;
    let mut steps: Vec<String> = records
        .iter()
        .filter_map(|r| {
            let p::Record::Item { item } = r else { return None };
            let said = match (item.kind.as_str(), &item.tool) {
                ("tool", Some(tool)) => tool.title.clone(),
                ("agent" | "thought", _) => item.content.iter().filter_map(|c| c.text.as_deref()).collect::<Vec<_>>().join("\n"),
                _ => return None,
            };
            // a report is JSON, often fenced: its braces are not a step
            let line = said.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with(['{', '`']))?;
            Some(if line.chars().count() > 160 { format!("{}…", line.chars().take(159).collect::<String>()) } else { line.to_string() })
        })
        .collect();
    steps.dedup();
    steps.drain(..steps.len().saturating_sub(KEPT));
    steps
}

#[derive(Debug)]
struct Report {
    summary: String,
    problems: Vec<api::Problem>,
}
/// Read the Reviewer's answer. Anything but the document asked for is an error, so that an
/// answer sill could not read is never shown as "no problems".
fn decode_report(text: &str) -> Result<Report> {
    #[derive(serde::Deserialize)]
    struct Raw {
        summary: String,
        problems: Vec<RawProblem>,
    }
    #[derive(serde::Deserialize)]
    struct RawProblem {
        title: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        line: Option<Value>,
        #[serde(default)]
        detail: Option<String>,
    }
    // the document alone, in a code fence, or with words around it
    let text = text.trim();
    let raw: Raw = serde_json::from_str(text)
        .ok()
        .or_else(|| {
            let (a, b) = (text.find('{')?, text.rfind('}')?);
            serde_json::from_str(text.get(a..=b)?).ok()
        })
        .context("The Reviewer's answer was not the report it was asked for")?;
    ensure!(!raw.summary.trim().is_empty(), "The Reviewer's report has no summary");
    let problems = raw
        .problems
        .into_iter()
        .take(api::MAX_PROBLEMS)
        .enumerate()
        .map(|(i, f)| {
            ensure!(!f.title.trim().is_empty(), "The Reviewer's report has a problem without words");
            // a place outside the repository is no place
            let path = f
                .path
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty() && Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_))));
            let line = f.line.and_then(|l| l.as_u64()).and_then(|l| u32::try_from(l).ok()).filter(|l| *l > 0 && path.is_some());
            Ok(api::Problem {
                id: format!("r0-p{i}"),
                reviewer_index: 0,
                title: bounded(f.title.trim(), 600),
                path,
                line,
                detail: bounded(f.detail.unwrap_or_default().trim(), 4_000),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Report { summary: bounded(raw.summary.trim(), 2_000), problems })
}
fn prompt(run: &api::Run, copy: &Path, asked: &str) -> String {
    format!(
"You are reviewing another agent's finished work, as a second opinion for the person who asked for it. Work only in {copy}: a disposable copy of the repository with the work checked out. You may build and run tests there. Do not fix anything, do not commit, and do not ask questions: say what you could not check instead.

The work is the difference between refs/crosscheck/base ({base}) and the files as they are (`git diff refs/crosscheck/base`). Review that difference and nothing else; read the code around it and the repository's own instructions as you need.

What the person asked for in that session:
{asked}

Look for concrete problems: the work not doing what was asked, defects, regressions, something asked for and missing. Leave out matters of taste.

Answer with exactly one JSON document and nothing else:
{{\"summary\": \"at most three plain sentences: does the work do what was asked, and what matters most\", \"problems\": [{{\"title\": \"the problem in one plain sentence\", \"path\": \"path/in/the/repository or null\", \"line\": 1, \"detail\": \"what is expected, what happens instead, and how you know\"}}]}}
At most {most} problems, the most important first; an empty list when you found none. The person reads the summary and the titles first, so write them for someone who has not seen the code. You have {minutes} minutes.",
        copy = copy.display(), base = run.base, asked = if asked.trim().is_empty() { "(nothing recorded)" } else { asked },
        most = api::MAX_PROBLEMS, minutes = api::TIME_LIMIT_SECS / 60)
}

// A copy of a clone must never show a path hidden in the jail that produced it.
fn masks_intersect(source: &Path, masks: &[PathBuf]) -> bool {
    masks.iter().any(|mask| source.starts_with(mask) || mask.starts_with(source))
}

#[cfg(test)]
#[path = "crosscheck/tests.rs"]
mod tests;
