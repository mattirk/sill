//! Host control's actions in the conversation, shown as tool runs are (the person, 2026-11-13):
//! consecutive Host notices make one block of their own; within it the actions are grouped by
//! operation (Run Mac commands, View Mac screen…), each action one line — its request and its
//! outcome together, a ✓ or ✕ — that opens to the full request and result.
//!
//! The daemon writes an action as two notices (`daemon/host_bridge.rs`): the request,
//! `HOST action on host · generation G: OP ARGS`, then its outcome, `HOST action OP completed: R`
//! or `HOST action OP failed: E`. A managed process's end is `HOST process host:ID …`.
use sill_proto::sfp::Item;

/// The text of a Host notice, when `item` is one.
pub fn host_text(item: &Item) -> Option<&str> {
    if item.kind != "notice" {
        return None;
    }
    let text = item.content.iter().find_map(|b| b.text.as_deref())?;
    (text.starts_with("HOST action ") || text.starts_with("HOST process ")).then_some(text)
}

/// One Host action: its request and outcome, from one or two notices.
#[derive(Debug, Clone, PartialEq)]
pub struct Action<'a> {
    pub op: String,
    /// Its place in the open set: the first notice's item id.
    pub key: String,
    /// The request's arguments as the daemon recorded them (JSON).
    pub request: Option<&'a str>,
    pub outcome: Option<Outcome<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome<'a> {
    Done(&'a str),
    Failed(&'a str),
}

impl Action<'_> {
    /// The status mark's status: as a tool call's.
    pub fn status(&self) -> &'static str {
        match self.outcome {
            Some(Outcome::Done(_)) => "completed",
            Some(Outcome::Failed(_)) => "failed",
            None => "in_progress",
        }
    }
}

/// The notices `(item id, text)` as actions, in order: each outcome joins the latest request of
/// its operation still without one.
pub fn actions<'a>(notices: &[(&'a str, &'a str)]) -> Vec<Action<'a>> {
    let mut out: Vec<Action<'a>> = Vec::new();
    for &(id, text) in notices {
        if let Some(rest) = text.strip_prefix("HOST action on host · generation ")
            && let Some((_, action)) = rest.split_once(": ")
        {
            let (op, args) = action.split_once(' ').unwrap_or((action, ""));
            out.push(Action { op: op.into(), key: id.into(), request: Some(args), outcome: None });
            continue;
        }
        if let Some(rest) = text.strip_prefix("HOST action ") {
            let parsed = rest
                .split_once(" completed: ")
                .map(|(op, r)| (op, Outcome::Done(r)))
                .or_else(|| rest.split_once(" failed: ").map(|(op, e)| (op, Outcome::Failed(e))));
            if let Some((op, outcome)) = parsed {
                match out.iter_mut().rev().find(|a| a.op == op && a.outcome.is_none()) {
                    Some(action) => action.outcome = Some(outcome),
                    None => out.push(Action { op: op.into(), key: id.into(), request: None, outcome: Some(outcome) }),
                }
                continue;
            }
        }
        // a managed process's end, or anything else Host said: an action of its own
        let failed = text.contains("revocation stop") || text.contains("Err(") || text.contains("signal");
        out.push(Action {
            op: "process".into(),
            key: id.into(),
            request: None,
            outcome: Some(if failed { Outcome::Failed(text) } else { Outcome::Done(text) }),
        });
    }
    out
}

/// The actions of one operation, under its name.
#[derive(Debug)]
pub struct Group<'a> {
    pub op: String,
    pub actions: Vec<Action<'a>>,
}

impl Group<'_> {
    /// As a tool group: running while any runs, then failed if any failed, else completed.
    pub fn status(&self) -> &'static str {
        let has = |s: &str| self.actions.iter().any(|a| a.status() == s);
        if has("in_progress") {
            "in_progress"
        } else if has("failed") {
            "failed"
        } else {
            "completed"
        }
    }
}

/// The actions grouped by operation, in the order each operation first came.
pub fn groups(actions: Vec<Action<'_>>) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for action in actions {
        match groups.iter_mut().find(|g| g.op == action.op) {
            Some(group) => group.actions.push(action),
            None => groups.push(Group { op: action.op.clone(), actions: vec![action] }),
        }
    }
    groups
}

/// What an action's line says: the command it ran, the picture it took, what it typed.
pub fn summary(action: &Action) -> String {
    let request = action.request.and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok());
    let said = match (action.op.as_str(), request.as_ref()) {
        ("commands" | "launch", Some(r)) => argv(&r["argv"]),
        ("mac_commands", Some(r)) => {
            if let Some(run) = r.get("run") {
                argv(&run["argv"])
            } else if r.get("list").is_some() {
                Some("ps".into())
            } else if let Some(name) = r["output"].as_str() {
                Some(format!("output {name}"))
            } else {
                r["stop"].as_str().map(|name| format!("stop {name}"))
            }
        }
        ("mac_view" | "view", Some(r)) => Some(if let Some(list) = r["list"].as_str() {
            list.to_string()
        } else if r["list"].as_bool() == Some(true) {
            "windows".into()
        } else if let Some(id) = r["display"].as_u64() {
            format!("screenshot display {id}")
        } else if let Some(id) = r["window"].as_u64() {
            format!("screenshot window {id}")
        } else {
            "screenshot".into()
        }),
        ("mac_input" | "input", Some(r)) => r["actions"].as_array().map(|actions| {
            actions
                .iter()
                .filter_map(|a| {
                    let (kind, value) = a.as_object()?.iter().find(|(k, _)| !matches!(k.as_str(), "button" | "count" | "dx" | "dy"))?;
                    Some(match value {
                        serde_json::Value::String(s) => format!("{kind} {s}"),
                        serde_json::Value::Array(p) if p.len() == 2 && p[0].is_number() => {
                            format!("{kind} {},{}", p[0], p[1])
                        }
                        _ => kind.clone(),
                    })
                })
                .collect::<Vec<_>>()
                .join(" · ")
        }),
        ("stop", Some(r)) => r["name"].as_str().map(str::to_owned),
        ("stop_existing", Some(r)) => r["pid"].as_u64().map(|pid| format!("pid {pid}")),
        _ => None,
    };
    said.or_else(|| match action.outcome {
        Some(Outcome::Done(t) | Outcome::Failed(t)) if action.request.is_none() => Some(t.to_string()),
        _ => None,
    })
    .unwrap_or_default()
}

/// An argv as one would type it; a shell's script as itself.
fn argv(value: &serde_json::Value) -> Option<String> {
    let words: Vec<&str> = value.as_array()?.iter().filter_map(|v| v.as_str()).collect();
    if words.len() == 3 && (words[1] == "-c" || words[1] == "-lc") {
        return Some(words[2].to_owned());
    }
    Some(
        words
            .iter()
            .map(|word| {
                if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_-./=:".contains(c)) {
                    (*word).to_owned()
                } else {
                    format!("'{}'", word.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_outcomes_pair_up_and_group_by_operation() {
        let notices = [
            ("a", r#"HOST action on host · generation 1: mac_commands {"run":{"argv":["cargo","build"],"cwd":"/Users/me/sill"}}"#),
            ("b", r#"HOST action mac_commands completed: {"name":"mac:1"}"#),
            ("c", r#"HOST action on host · generation 1: mac_view {"display":1}"#),
            ("d", r#"HOST action on host · generation 1: mac_commands {"output":"mac:1"}"#),
            ("e", "HOST action mac_view failed: macOS does not allow Sill to record the screen"),
            ("f", r#"HOST action mac_commands completed: {"output":"Finished"}"#),
            ("g", r#"HOST action on host · generation 1: mac_commands {"list":true}"#),
        ];
        let actions = actions(&notices);
        assert_eq!(actions.len(), 4);
        assert_eq!((actions[0].key.as_str(), actions[0].status()), ("a", "completed"));
        assert_eq!(actions[1].status(), "failed");
        assert_eq!(actions[2].outcome, Some(Outcome::Done(r#"{"output":"Finished"}"#)));
        assert_eq!(actions[3].status(), "in_progress", "a request without its outcome yet");
        let summaries: Vec<String> = actions.iter().map(summary).collect();
        assert_eq!(summaries, ["cargo build", "screenshot display 1", "output mac:1", "ps"]);
        let groups = groups(actions);
        assert_eq!(groups.iter().map(|g| g.op.as_str()).collect::<Vec<_>>(), ["mac_commands", "mac_view"]);
        assert_eq!(groups[0].actions.len(), 3);
        assert_eq!((groups[0].status(), groups[1].status()), ("in_progress", "failed"));
    }

    #[test]
    fn summaries_say_what_was_done() {
        let one = |op: &str, request: &'static str| summary(&Action { op: op.into(), key: "k".into(), request: Some(request), outcome: None });
        assert_eq!(one("commands", r#"{"argv":["/bin/sh","-c","echo hi"]}"#), "echo hi");
        assert_eq!(one("mac_commands", r#"{"run":{"argv":["/bin/zsh","-lc","cd /tmp && make"],"cwd":"/x"}}"#), "cd /tmp && make");
        assert_eq!(one("mac_commands", r#"{"run":{"argv":["ls","two words"],"cwd":"/x"}}"#), "ls 'two words'");
        assert_eq!(one("mac_commands", r#"{"stop":"mac:3"}"#), "stop mac:3");
        assert_eq!(one("mac_view", r#"{"list":"windows"}"#), "windows");
        assert_eq!(
            one("mac_input", r#"{"frame":"f","actions":[{"click":[820,410],"button":"right"},{"type":"hello"},{"key":"return"}]}"#),
            "click 820,410 · type hello · key return"
        );
        let ended = actions(&[("p", "HOST process host:4 ended: Ok(ExitStatus(0))")]);
        assert_eq!((ended[0].op.as_str(), ended[0].status()), ("process", "completed"));
    }
}
