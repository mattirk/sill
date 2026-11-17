//! Crosscheck (`sill.crosscheck: 1`, docs/crosscheck.md): a second opinion on the work waiting in
//! a workspace. One Reviewer reads a capture of it and reports a short summary and a list of
//! problems. The daemon owns the capture, the Reviewer's worker session and the stored review;
//! a frontend starts one, shows it and removes it.
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "sill.crosscheck";
/// How long a review may take, capture to answer.
pub const TIME_LIMIT_SECS: u64 = 1200;
/// A Reviewer is told to report at most this many problems; what it reports beyond them is left out.
pub const MAX_PROBLEMS: usize = 7;

/// Who reviews: an agent and that agent's own options. Its options are the Reviewer's alone: they
/// do not change what the person's ordinary sessions of that agent start with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckReviewer")]
pub struct Reviewer {
    /// What the person calls it; the agent's id when they gave it no name.
    #[serde(default)]
    pub name: String,
    pub agent: String,
    /// Option id → value id for a choice, `true`/`false` for a switch (the ids of
    /// `SessionControls::options`). An option left out stays as the agent starts with it.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

/// The Reviewers setting and what can be chosen for it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckReviewers")]
pub struct Reviewers {
    /// The saved Reviewers, the one a review uses first. Empty: none was chosen, and a review is
    /// done by an agent other than the session's own when there is one.
    pub reviewers: Vec<Reviewer>,
    /// Every agent that can review, with its options as its latest session reported them.
    pub agents: Vec<AgentOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckAgentOptions")]
pub struct AgentOptions {
    pub agent: String,
    pub title: String,
    /// Empty when no session of this agent has reported its options here yet: it then reviews
    /// as it starts.
    pub options: Vec<crate::sfp::ControlOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CrosscheckSetReviewers")]
pub struct SetReviewers {
    pub reviewers: Vec<Reviewer>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CrosscheckEmpty")]
pub struct Empty {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CrosscheckWorkspaceKey")]
pub struct WorkspaceKey {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CrosscheckStart")]
pub struct Start {
    /// Chosen by the frontend for this press of the button: a repeated start with it answers
    /// with the review it started and starts nothing.
    pub operation_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CrosscheckRunKey")]
pub struct RunKey {
    pub run_id: String,
}

/// A workspace's review, when it has one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckCurrent")]
pub struct Current {
    pub run: Option<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckRun")]
pub struct Run {
    pub run_id: String,
    pub workspace_id: String,
    /// The session whose work it is.
    pub session_id: String,
    /// The reviewed files as a Git tree, uncommitted work included (`Detail::state_tree`).
    pub state_tree: String,
    /// The commit the review compares against: where the work leaves the person's checkout, so
    /// the review covers what an Apply would bring and nothing else.
    pub base: String,
    /// `capturing`, `reviewing`, `complete` or `failed`. Only `complete` is a review; `failed`
    /// says why in `detail`.
    pub state: String,
    pub detail: String,
    /// Who reviewed. One in this version.
    pub reviewers: Vec<Participant>,
    /// What they found, the most important first. Empty on a complete review: no problems found.
    pub problems: Vec<Problem>,
    /// Unix seconds, as a decimal string.
    pub created_at: String,
    /// The workspace's files are no longer the ones reviewed.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckParticipant")]
pub struct Participant {
    pub reviewer: Reviewer,
    /// Its worker session, once made.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `starting`, `running`, `complete` or `failed`.
    pub state: String,
    /// Its assessment in at most three sentences.
    #[serde(default)]
    pub summary: String,
    /// Its latest steps, one line each, the latest last.
    #[serde(default)]
    pub activity: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckProblem")]
pub struct Problem {
    pub id: String,
    /// Which of `Run::reviewers` reported it.
    pub reviewer_index: u32,
    /// The problem in one sentence.
    pub title: String,
    /// Relative to the repository.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    /// The Reviewer's explanation: what is expected, what happens, how it knows.
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "CrosscheckNotice")]
pub struct Notice {
    pub workspace_id: String,
    pub run_id: String,
}

/// What "Ask the agent to fix these" sends to the session whose work it is, as a submission with
/// the purpose [`FIX_PURPOSE`]. Its first line is what a frontend shows for it.
pub fn fix_request(run: &Run) -> String {
    let who = run.reviewers.first().map_or("the Reviewer", |p| reviewer_name(&p.reviewer));
    let n = run.problems.len();
    let mut text = format!(
        "Fix the {n} problem{} {who} found\n\nA second opinion on this work (Crosscheck, by {who}) reported the problem{} below. For each, check that it is real and still applies, fix it if so, and say what you did or why it needs nothing.\n",
        if n == 1 { "" } else { "s" },
        if n == 1 { "" } else { "s" },
    );
    for (i, p) in run.problems.iter().enumerate() {
        text.push_str(&format!("\n{}. {}\n", i + 1, p.title));
        if let Some(path) = &p.path {
            text.push_str(&match p.line {
                Some(line) => format!("   Where: {path}:{line}\n"),
                None => format!("   Where: {path}\n"),
            });
        }
        if !p.detail.trim().is_empty() {
            text.push_str(&format!("   {}\n", p.detail.trim().replace('\n', "\n   ")));
        }
    }
    text
}

/// A submission's purpose (`Submit::purpose`) for [`fix_request`].
pub const FIX_PURPOSE: &str = "crosscheck";

pub fn reviewer_name(r: &Reviewer) -> &str {
    if r.name.trim().is_empty() { &r.agent } else { r.name.trim() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fix_request_names_who_found_what_and_where() {
        let run = Run {
            run_id: "r".into(),
            workspace_id: "w".into(),
            session_id: "s".into(),
            state_tree: "t".into(),
            base: "b".into(),
            state: "complete".into(),
            detail: String::new(),
            reviewers: vec![Participant {
                reviewer: Reviewer { name: String::new(), agent: "codex".into(), options: BTreeMap::new() },
                session_id: None,
                state: "complete".into(),
                summary: "Fine.".into(),
                activity: vec![],
                error: None,
            }],
            problems: vec![
                Problem {
                    id: "r0-p0".into(),
                    reviewer_index: 0,
                    title: "The test accepts too much.".into(),
                    path: Some("src/jail.rs".into()),
                    line: Some(12),
                    detail: "A layer inside the state directory would pass.\nSeen by reading.".into(),
                },
                Problem { id: "r0-p1".into(), reviewer_index: 0, title: "A comment is stale.".into(), path: None, line: None, detail: String::new() },
            ],
            created_at: "1".into(),
            stale: false,
        };
        let text = fix_request(&run);
        assert_eq!(text.lines().next(), Some("Fix the 2 problems codex found"));
        assert!(
            text.contains(
                "\n1. The test accepts too much.\n   Where: src/jail.rs:12\n   A layer inside the state directory would pass.\n   Seen by reading.\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("\n2. A comment is stale.\n"), "{text}");
    }
}
