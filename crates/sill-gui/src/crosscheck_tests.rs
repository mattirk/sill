use super::*;
use sill_client::crosscheck::Seen;

/// A review as the daemon reports it, for tests and the snapshot renders.
pub(crate) fn run(state: &str, stale: bool, problems: &[(&str, Option<(&str, u32)>)], steps: &[&str]) -> api::Run {
    let now = now_secs();
    api::Run {
        run_id: format!("run-{state}"), workspace_id: "ws-1".into(), session_id: "demo".into(), state_tree: "tree".into(), base: "base".into(),
        state: state.into(),
        detail: if state == "failed" { "Stopped after 20 minutes".into() } else { String::new() },
        reviewers: vec![api::Participant {
            reviewer: api::Reviewer { name: String::new(), agent: "codex".into(), options: Default::default() },
            session_id: Some("worker".into()), state: "running".into(),
            summary: if state == "complete" {
                "The change does what was asked: the layers test no longer fails where /mnt holds a sill-named directory. Two things are worth a look before applying; neither changes how jails are built.".into()
            } else { String::new() },
            activity: steps.iter().map(|s| s.to_string()).collect(),
            error: (state == "failed").then(|| "Stopped after 20 minutes: the Reviewer was still running the jail tests".into()),
        }],
        problems: problems.iter().enumerate().map(|(i, (title, place))| api::Problem {
            id: format!("r0-p{i}"), reviewer_index: 0, title: title.to_string(), path: place.map(|(p, _)| p.to_string()), line: place.map(|(_, l)| l), detail: "Why.".into(),
        }).collect(),
        created_at: (now - 75).to_string(), stale,
    }
}

fn seen(run: Option<api::Run>) -> Result<Seen, String> {
    Ok(Seen { offered: true, run })
}

pub(crate) fn setup() -> api::Reviewers {
    let option = |id: &str, title: &str, value: &str, choices: &[(&str, &str)]| -> ControlOption {
        serde_json::from_value(serde_json::json!({"option_id": id, "title": title, "category": id, "value": {"type": "id", "value": value},
            "choices": choices.iter().map(|(v, t)| serde_json::json!({"value": v, "title": t, "description": ""})).collect::<Vec<_>>()})).unwrap()
    };
    api::Reviewers {
        reviewers: vec![],
        agents: vec![
            api::AgentOptions { agent: "claude".into(), title: "Claude Code".into(), options: vec![] },
            api::AgentOptions { agent: "codex".into(), title: "Codex".into(), options: vec![
                option("model", "Model", "small", &[("small", "Small"), ("large", "Large")]),
                option("effort", "Reasoning effort", "medium", &[("low", "Low"), ("medium", "Medium"), ("high", "High")]),
            ] },
        ],
    }
}

#[test]
fn a_review_is_shown_as_read_and_the_cross_removes_it_at_once() {
    let (mut app, _) = App::new(true, None);
    assert!(!app.crosscheck.offered && !app.crosscheck.ticking());
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(Some(run("reviewing", false, &[], &["Read AGENTS.md"])))));
    assert!(app.crosscheck.offered && app.crosscheck.ticking(), "a Reviewer at work is read again");
    let _ = app.crosscheck_event(Event::Toggle("ws-1".into()));
    assert!(app.crosscheck.open.contains("ws-1"));
    // the cross: gone from the line before the daemon answers
    let _ = app.crosscheck_event(Event::Remove("ws-1".into()));
    assert!(app.crosscheck.runs.is_empty() && app.crosscheck.open.is_empty() && !app.crosscheck.ticking());
    // an answer read before the removal does not bring the review back
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(Some(run("reviewing", false, &[], &[])))));
    assert!(app.crosscheck.runs.is_empty());
    // a new review of the same workspace is another one
    let mut next = run("complete", false, &[], &[]);
    next.run_id = "run-next".into();
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(Some(next))));
    assert_eq!(app.crosscheck.runs["ws-1"].run_id, "run-next");
    assert!(!app.crosscheck.ticking(), "a finished review is not read again");
    // the daemon has none any more: neither has the line
    let _ = app.crosscheck_event(Event::Toggle("ws-1".into()));
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(None)));
    assert!(app.crosscheck.runs.is_empty() && app.crosscheck.open.is_empty());
    // a failed read leaves the line as it was
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(Some(run("complete", false, &[], &[])))));
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), Err("offline".into())));
    assert!(app.crosscheck.runs.contains_key("ws-1"));
}

#[test]
fn a_start_that_fails_says_why_and_a_started_review_is_shown() {
    let (mut app, _) = App::new(true, None);
    app.crosscheck.starting.insert("ws-1".into());
    assert!(app.crosscheck.ticking());
    // the daemon has nothing yet for a start on its way: the line does not flicker away
    let _ = app.crosscheck_event(Event::Loaded("ws-1".into(), seen(None)));
    assert!(app.crosscheck.starting.contains("ws-1"));
    let _ = app.crosscheck_event(Event::Started("ws-1".into(), Err("There is nothing to review: the workspace has no work waiting".into())));
    assert!(app.crosscheck.starting.is_empty());
    assert!(app.crosscheck.said["ws-1"].contains("nothing to review"));
    app.crosscheck.starting.insert("ws-1".into());
    let _ = app.crosscheck_event(Event::Started("ws-1".into(), Ok(run("capturing", false, &[], &[]))));
    assert!(app.crosscheck.starting.is_empty() && app.crosscheck.ticking());
    assert_eq!(app.crosscheck.runs["ws-1"].state, "capturing");
}

#[test]
fn the_reviewer_setting_is_an_agent_and_that_agents_options() {
    let (mut app, _) = App::new(true, None);
    let _ = app.crosscheck_event(Event::Reviewers(Ok(setup())));
    let saved = |app: &App| match &app.crosscheck.reviewers { Some(Ok(s)) => s.reviewers.clone(), other => panic!("{other:?}") };
    // an option without a Reviewer is nobody's
    let _ = app.crosscheck_event(Event::Choose("model".into(), "large".into()));
    assert!(saved(&app).is_empty());
    let _ = app.crosscheck_event(Event::Agent(Some("codex".into())));
    // every option the agent shows is the Reviewer's from then on: what is shown is what is used
    assert_eq!(saved(&app)[0].options, [("effort".to_string(), "medium".to_string()), ("model".to_string(), "small".to_string())].into());
    let _ = app.crosscheck_event(Event::Choose("model".into(), "large".into()));
    let _ = app.crosscheck_event(Event::Choose("effort".into(), "high".into()));
    assert_eq!(saved(&app)[0].options, [("effort".to_string(), "high".to_string()), ("model".to_string(), "large".to_string())].into());
    // the same agent again changes nothing; another agent starts from its own options
    let _ = app.crosscheck_event(Event::Agent(Some("codex".into())));
    assert_eq!(saved(&app)[0].options.len(), 2);
    let _ = app.crosscheck_event(Event::Agent(Some("claude".into())));
    assert_eq!(saved(&app), [api::Reviewer { name: String::new(), agent: "claude".into(), options: Default::default() }]);
    let _ = app.crosscheck_event(Event::Agent(None));
    assert!(saved(&app).is_empty(), "Automatic is no saved Reviewer");
    let codex = &setup().agents[1];
    let reviewer = api::Reviewer { name: String::new(), agent: "codex".into(), options: [("model".to_string(), "large".to_string())].into() };
    assert_eq!(chosen(Some(&reviewer), &codex.options[0]), "large");
    assert_eq!(chosen(Some(&reviewer), &codex.options[1]), "medium", "what the agent starts with");
    assert_eq!(chosen(None, &codex.options[0]), "small");
}

#[test]
fn only_a_current_review_with_problems_is_sent_to_be_fixed() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=crate::demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let _ = app.update(crate::tests::in_session(&app, crate::session::Event::Answer { question_id: "demo-question".into(), option_id: "allow".into() }));
    let sent = |app: &App| app.views["demo"].outbox.pending().iter().filter(|p| p.submit.purpose.as_deref() == Some(api::FIX_PURPOSE)).count();
    let before = sent(&app);
    for (state, stale, problems) in [("complete", false, 0), ("complete", true, 2), ("reviewing", false, 0), ("failed", false, 0)] {
        let problems: Vec<(&str, Option<(&str, u32)>)> = (0..problems).map(|_| ("A problem.", None)).collect();
        app.crosscheck.runs.insert("ws-1".into(), run(state, stale, &problems, &[]));
        let _ = app.crosscheck_event(Event::Fix("demo".into(), "ws-1".into()));
        assert_eq!(sent(&app), before, "{state} stale={stale}");
    }
    app.crosscheck.runs.insert("ws-1".into(), run("complete", false, &[("A problem.", None), ("Another.", Some(("src/a.rs", 3)))], &[]));
    app.crosscheck.open.insert("ws-1".into());
    let _ = app.crosscheck_event(Event::Fix("demo".into(), "ws-1".into()));
    assert_eq!(sent(&app), before + 1);
    assert!(app.crosscheck.open.is_empty(), "the line closes once its problems are sent");
}
