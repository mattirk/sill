use super::*;

fn item(kind: &str, text: &str, tool: Option<&str>) -> p::Record {
    p::Record::Item { item: serde_json::from_value(json!({
        "item_id": "i", "created_seq": "1", "turn_id": "t", "submission_id": null, "agent_generation_id": "g", "kind": kind,
        "content": [{"block_id": "b", "type": "text", "text": text}],
        "tool": tool.map(|title| json!({"title": title, "kind": "read", "status": "completed", "detail": "", "diff": "", "parent_item_id": null, "command": null})),
    })).unwrap() }
}

#[test]
fn only_the_report_asked_for_is_a_review() {
    let valid = r#"{"summary":"It does what was asked.","problems":[]}"#;
    for text in [valid.to_string(), format!("```json\n{valid}\n```"), format!("```\n{valid}\n```"), format!("Here is my report:\n{valid}\nThat is all.")] {
        let report = decode_report(&text).unwrap();
        assert_eq!(report.summary, "It does what was asked.");
        assert!(report.problems.is_empty());
    }
    // an answer silli cannot read is never "no problems"
    for invalid in [
        "Looks good to me",
        "{}",
        r#"{"problems":[]}"#,
        r#"{"summary":"fine","problems":null}"#,
        r#"{"summary":"  ","problems":[]}"#,
        r#"{"summary":"fine","problems":[{"title":" "}]}"#,
        r#"{"summary":"fine""#,
    ] {
        assert!(decode_report(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn problems_keep_their_order_and_only_places_inside_the_repository() {
    let report = decode_report(
        r#"{"summary":"Two things.","problems":[
        {"title":"First.","path":"src/lib.rs","line":12,"detail":"Why."},
        {"title":"Second.","path":"/etc/passwd","line":3},
        {"title":"Third.","path":"../outside","line":"7"},
        {"title":"Fourth.","path":null,"line":4,"detail":null}]}"#,
    )
    .unwrap();
    let got: Vec<_> = report.problems.iter().map(|p| (p.id.as_str(), p.title.as_str(), p.path.as_deref(), p.line)).collect();
    assert_eq!(
        got,
        [
            ("r0-p0", "First.", Some("src/lib.rs"), Some(12)),
            ("r0-p1", "Second.", None, None),
            ("r0-p2", "Third.", None, None),
            ("r0-p3", "Fourth.", None, None)
        ]
    );
    assert_eq!(report.problems[0].detail, "Why.");
    let many = format!(r#"{{"summary":"Many.","problems":[{}]}}"#, (0..12).map(|i| format!(r#"{{"title":"Problem {i}."}}"#)).collect::<Vec<_>>().join(","));
    let report = decode_report(&many).unwrap();
    assert_eq!(report.problems.len(), api::MAX_PROBLEMS);
    assert_eq!(report.problems.last().unwrap().title, "Problem 6.");
}

#[test]
fn steps_are_what_the_reviewer_did_and_the_answer_is_its_last_message() {
    let records = vec![
        item("user", "You are reviewing another agent's finished work", None),
        item("agent", "Comparing the work with its base.", None),
        item("tool", "", Some("Read src/lib.rs")),
        item("tool", "", Some("Read src/lib.rs")),
        item("thought", "\n  The test looks too broad.\nMore.", None),
        item("agent", "```json\n{\"summary\":\"Fine.\",\"problems\":[]}\n```", None),
    ];
    assert_eq!(activity(&records), ["Comparing the work with its base.", "Read src/lib.rs", "The test looks too broad."]);
    assert!(answer(&records).contains("\"summary\":\"Fine.\""));
    assert_eq!(answer(&records[..1]), "");
    let long = item("agent", &"x".repeat(400), None);
    assert_eq!(activity(&[long])[0].chars().count(), 160);
}

#[test]
fn the_prompt_names_the_copy_the_base_and_what_was_asked() {
    let run = api::Run {
        run_id: "run".into(),
        workspace_id: "workspace".into(),
        session_id: "origin".into(),
        state_tree: "tree".into(),
        base: "abc123".into(),
        state: "reviewing".into(),
        detail: String::new(),
        reviewers: vec![],
        problems: vec![],
        created_at: "0".into(),
        stale: false,
    };
    let text = prompt(&run, Path::new("/data/crosschecks/run/review-0"), "make the test pass");
    for part in ["/data/crosschecks/run/review-0", "refs/crosscheck/base (abc123)", "make the test pass", "\"problems\"", "At most 7 problems", "20 minutes"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert!(prompt(&run, Path::new("/c"), " ").contains("(nothing recorded)"));
}

#[test]
fn a_failed_review_says_why_and_reviewers_are_bounded() {
    let mut run = api::Run {
        run_id: "run".into(),
        workspace_id: "workspace".into(),
        session_id: "origin".into(),
        state_tree: "tree".into(),
        base: "base".into(),
        state: "reviewing".into(),
        detail: String::new(),
        reviewers: vec![api::Participant {
            reviewer: api::Reviewer { name: String::new(), agent: "codex".into(), options: Default::default() },
            session_id: None,
            state: "running".into(),
            summary: String::new(),
            activity: vec!["Read a file".into()],
            error: None,
        }],
        problems: vec![],
        created_at: "0".into(),
        stale: false,
    };
    fail(&mut run, &anyhow::anyhow!("inner cause").context("Stopped after 20 minutes"));
    assert_eq!((run.state.as_str(), run.detail.as_str(), run.reviewers[0].state.as_str()), ("failed", "Stopped after 20 minutes", "failed"));
    assert_eq!(run.reviewers[0].error.as_deref(), Some("Stopped after 20 minutes: inner cause"));
    assert!(!active(&run.state) && active("capturing") && active("reviewing") && !active("complete"));

    let reviewer = |agent: &str, name: &str| api::Reviewer { name: name.into(), agent: agent.into(), options: [("model".to_string(), "x".to_string())].into() };
    assert!(validate_reviewers(&[]).is_ok());
    assert!(validate_reviewers(&[reviewer("codex", "Careful")]).is_ok());
    assert!(validate_reviewers(&[reviewer("", "")]).is_err());
    assert!(validate_reviewers(&[reviewer("codex", "a\nb")]).is_err());
    assert!(validate_reviewers(&vec![reviewer("codex", ""); 17]).is_err());
    assert!(masks_intersect(Path::new("/home/me/repo"), &[PathBuf::from("/home/me/repo/.env")]));
    assert!(!masks_intersect(Path::new("/home/me/repo"), &[PathBuf::from("/home/me/secrets")]));
}
