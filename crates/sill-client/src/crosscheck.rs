//! Blocking Crosscheck calls for a frontend worker (docs/crosscheck.md). A start is sent once: a
//! lost connection means reading the workspace's review before deciding to start again.
use crate::actions::Actions;
use sill_proto::crosscheck as api;
use sill_proto::method;

pub fn reviewers(actions: &Actions) -> anyhow::Result<api::Reviewers> {
    actions.reading(|c| c.call(method::CROSSCHECK_REVIEWERS, api::Empty {}))
}

pub fn set_reviewers(actions: &Actions, reviewers: &[api::Reviewer]) -> anyhow::Result<api::Reviewers> {
    actions.once(|c| c.call(method::CROSSCHECK_SET_REVIEWERS, api::SetReviewers { reviewers: reviewers.into() }))
}

/// A workspace's review as a frontend asks for it: whether this daemon does Crosscheck at all,
/// and the review when there is one.
#[derive(Debug, Clone)]
pub struct Seen {
    pub offered: bool,
    pub run: Option<api::Run>,
}

pub fn get(actions: &Actions, workspace_id: &str) -> anyhow::Result<Seen> {
    actions.reading(|c| {
        if !c.has_capability(api::CAPABILITY) {
            return Ok(Seen { offered: false, run: None });
        }
        let current: api::Current = c.call(method::CROSSCHECK_GET, api::WorkspaceKey { workspace_id: workspace_id.into() })?;
        Ok(Seen { offered: true, run: current.run })
    })
}

pub fn start(actions: &Actions, request: &api::Start) -> anyhow::Result<api::Run> {
    actions.once(|c| c.call(method::CROSSCHECK_START, request))
}

pub fn remove(actions: &Actions, run_id: &str) -> anyhow::Result<()> {
    actions.once(|c| c.call::<serde_json::Value>(method::CROSSCHECK_REMOVE, api::RunKey { run_id: run_id.into() })).map(|_| ())
}

/// Where a review stands, as the shelf's line needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The Reviewer is at work.
    Working,
    /// A finished review of the files as they are.
    Done,
    /// A finished review of files that have changed since.
    Stale,
    /// It did not finish; `Run::detail` says why.
    Failed,
}

pub fn stage(run: &api::Run) -> Stage {
    match run.state.as_str() {
        "capturing" | "reviewing" => Stage::Working,
        "complete" if run.stale => Stage::Stale,
        "complete" => Stage::Done,
        _ => Stage::Failed,
    }
}

/// Who reviews, as the line names them.
pub fn who(run: &api::Run) -> &str {
    run.reviewers.first().map_or("The Reviewer", |p| api::reviewer_name(&p.reviewer))
}

/// The line's words after the Reviewer's name: "is reviewing", "found 2 problems".
pub fn verdict(run: &api::Run) -> String {
    match stage(run) {
        Stage::Working => "is reviewing".into(),
        Stage::Failed => "could not finish".into(),
        Stage::Done | Stage::Stale => match run.problems.len() {
            0 => "found no problems".into(),
            1 => "found 1 problem".into(),
            n => format!("found {n} problems"),
        },
    }
}

/// How long a review has run, for its line: "under a minute", "1 min", "12 min".
pub fn elapsed(run: &api::Run, now_secs: u64) -> String {
    let started: u64 = run.created_at.parse().unwrap_or(now_secs);
    match now_secs.saturating_sub(started) / 60 {
        0 => "under a minute".into(),
        n => format!("{n} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(state: &str, stale: bool, problems: usize) -> api::Run {
        api::Run {
            run_id: "run-1".into(),
            workspace_id: "ws-1".into(),
            session_id: "session-1".into(),
            state_tree: "tree".into(),
            base: "base".into(),
            state: state.into(),
            detail: String::new(),
            reviewers: vec![api::Participant {
                reviewer: api::Reviewer { name: String::new(), agent: "codex".into(), options: Default::default() },
                session_id: None,
                state: "complete".into(),
                summary: String::new(),
                activity: vec![],
                error: None,
            }],
            problems: (0..problems)
                .map(|i| api::Problem { id: format!("r0-p{i}"), reviewer_index: 0, title: "A problem.".into(), path: None, line: None, detail: String::new() })
                .collect(),
            created_at: "1000".into(),
            stale,
        }
    }

    #[test]
    fn the_line_says_who_and_what_at_every_stage() {
        let said = |r: &api::Run| format!("{} {}", who(r), verdict(r));
        assert_eq!((stage(&run("capturing", false, 0)), said(&run("capturing", false, 0)).as_str()), (Stage::Working, "codex is reviewing"));
        assert_eq!(stage(&run("reviewing", true, 0)), Stage::Working, "a review under way is not out of date yet");
        assert_eq!((stage(&run("complete", false, 0)), said(&run("complete", false, 0)).as_str()), (Stage::Done, "codex found no problems"));
        assert_eq!(said(&run("complete", false, 1)), "codex found 1 problem");
        assert_eq!((stage(&run("complete", true, 2)), said(&run("complete", true, 2)).as_str()), (Stage::Stale, "codex found 2 problems"));
        // nothing unknown is a clean review
        for state in ["failed", "interrupted", "anything"] {
            assert_eq!((stage(&run(state, false, 0)), said(&run(state, false, 0)).as_str()), (Stage::Failed, "codex could not finish"));
        }
        let mut named = run("complete", false, 0);
        named.reviewers[0].reviewer.name = " Careful ".into();
        assert_eq!(who(&named), "Careful");
    }

    #[test]
    fn elapsed_time_is_whole_minutes() {
        let r = run("reviewing", false, 0);
        assert_eq!(elapsed(&r, 1000), "under a minute");
        assert_eq!(elapsed(&r, 1059), "under a minute");
        assert_eq!(elapsed(&r, 1060), "1 min");
        assert_eq!(elapsed(&r, 1000 + 12 * 60 + 5), "12 min");
        assert_eq!(elapsed(&r, 10), "under a minute", "a clock behind the daemon's");
    }
}
