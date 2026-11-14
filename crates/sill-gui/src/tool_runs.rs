//! Consecutive tool calls shown as one block, grouped by what they run (docs/sill-gui.md, *Tool
//! runs*). A run is an unbroken sequence of tool calls in one turn: anything else — the agent's
//! text, a thought, the person's prompt, a question under a call — ends it, so nothing moves past
//! what the agent said or was asked. Within a run the calls are grouped by their program (`sed`,
//! `grep`) or, for calls that are not commands, by their kind (`read`, `edit`); the groups are in
//! the order their first call came, so a new call never moves a group already shown.
use std::ops::Range;

use sill_proto::sfp::{Item, Tool};

/// The feed's rows over `items`: each range one row, a run of tool calls when it holds more than
/// one item. `stands_alone` names items that must keep their own row (a call with a question
/// under it).
pub fn entries(items: &[&Item], stands_alone: impl Fn(&Item) -> bool) -> Vec<Range<usize>> {
    let joins = |item: &Item| item.tool.is_some() && !stands_alone(item);
    let mut entries: Vec<Range<usize>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match entries.last_mut() {
            Some(last)
                if joins(item)
                    && joins(items[last.end - 1])
                    && items[last.end - 1].turn_id == item.turn_id =>
            {
                last.end = index + 1;
            }
            _ => entries.push(index..index + 1),
        }
    }
    entries
}

/// The calls of a run that ran the same thing, under that thing's name.
pub struct Group<'a> {
    pub name: String,
    pub kind: &'a str,
    pub members: Vec<(&'a Item, &'a Tool)>,
}

impl Group<'_> {
    /// The status the group's line shows: running while any call runs, then failed if any failed,
    /// then waiting if any waits, else completed.
    pub fn status(&self) -> &'static str {
        let has = |wanted: &str| {
            self.members
                .iter()
                .any(|(_, tool)| normal_status(&tool.status) == wanted)
        };
        if has("inprogress") {
            "in_progress"
        } else if has("failed") {
            "failed"
        } else if self
            .members
            .iter()
            .any(|(_, tool)| normal_status(&tool.status) != "completed")
        {
            "pending"
        } else {
            "completed"
        }
    }

    /// The latest call's title after the group's name: what it ran this time.
    pub fn latest(&self) -> String {
        self.members
            .last()
            .map(|(_, tool)| classify(tool).1)
            .unwrap_or_default()
    }
}

fn normal_status(status: &str) -> String {
    status.to_ascii_lowercase().replace('_', "")
}

/// A run's calls grouped by name, in the order each name first came.
pub fn groups<'a>(run: &[&'a Item]) -> Vec<Group<'a>> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    for item in run {
        let Some(tool) = &item.tool else { continue };
        let (name, _) = classify(tool);
        let kind = tool.kind.as_str();
        match groups
            .iter_mut()
            .find(|g| g.name == name && g.kind.eq_ignore_ascii_case(kind))
        {
            Some(group) => group.members.push((item, tool)),
            None => groups.push(Group {
                name,
                kind,
                members: vec![(item, tool)],
            }),
        }
    }
    groups
}

/// A call's name and the rest of its title. A command is named by the program it runs: past
/// leading `cd …` steps and variable assignments, inside a shell's `-c` script, as the
/// program's file name. Other calls are named by their kind (`read`, `search`), or by their
/// title's first word when the kind says nothing (`other`).
pub fn classify(tool: &Tool) -> (String, String) {
    let title = tool.title.trim();
    let kind = tool.kind.to_ascii_lowercase();
    match kind.as_str() {
        "execute" => {
            let command = unquoted(title, '`');
            match program(command) {
                Some((name, rest)) => (name, rest.trim().to_owned()),
                None => (kind, title.to_owned()),
            }
        }
        "" | "other" => match first_word(title) {
            Some((word, rest)) => (word.to_owned(), rest.trim().to_owned()),
            None => ("other".into(), String::new()),
        },
        _ => {
            // "Read file '…'" under `read`: the word that repeats the kind is left out
            let rest = match first_word(title) {
                Some((word, rest)) if word.eq_ignore_ascii_case(&kind) => rest.trim(),
                _ => title,
            };
            (kind, rest.to_owned())
        }
    }
}

/// The program a command line runs and what follows it.
fn program(command: &str) -> Option<(String, &str)> {
    let mut command = command.trim_start();
    loop {
        let (word, rest) = first_word(command)?;
        if is_assignment(word) {
            command = rest.trim_start();
            continue;
        }
        if word == "cd" {
            // the step after `cd …`, when there is one; a lone `cd` is the program
            match [" && ", ";", "\n"]
                .iter()
                .filter_map(|sep| rest.find(sep).map(|at| at + sep.len()))
                .min()
            {
                Some(next) if !rest[next..].trim().is_empty() => {
                    command = rest[next..].trim_start();
                    continue;
                }
                _ => return Some(("cd".into(), rest)),
            }
        }
        let name = word.rsplit('/').next().unwrap_or(word);
        if matches!(name, "sh" | "bash" | "zsh" | "dash")
            && let Some((flags, script)) = first_word(rest)
            && flags.starts_with('-')
            && flags.contains('c')
        {
            let script = script.trim();
            let script = match script.chars().next() {
                Some(quote @ ('\'' | '"')) => unquoted(script, quote),
                _ => script,
            };
            command = script.trim_start();
            continue;
        }
        return Some((name.to_owned(), rest));
    }
}

/// `text` without the `quote` it starts with and the last one it has.
fn unquoted(text: &str, quote: char) -> &str {
    let Some(inner) = text.strip_prefix(quote) else {
        return text;
    };
    match inner.rfind(quote) {
        Some(end) => &inner[..end],
        None => inner,
    }
}

fn first_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((&text[..end], &text[end..]))
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(kind: &str, title: &str) -> Tool {
        Tool {
            title: title.into(),
            kind: kind.into(),
            status: "completed".into(),
            detail: String::new(),
            diff: String::new(),
            parent_item_id: None,
        }
    }

    fn named(kind: &str, title: &str) -> (String, String) {
        classify(&tool(kind, title))
    }

    #[test]
    fn a_command_is_named_by_the_program_it_runs() {
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            named("execute", "sed -n 960,1257p crates/x.rs"),
            pair("sed", "-n 960,1257p crates/x.rs")
        );
        assert_eq!(named("Execute", "`git status`"), pair("git", "status"));
        assert_eq!(
            named("execute", "cd /home/me/GIT/sill && grep -rn foo crates"),
            pair("grep", "-rn foo crates")
        );
        assert_eq!(
            named("execute", "RUST_LOG=debug cargo test -p sill-gui"),
            pair("cargo", "test -p sill-gui")
        );
        assert_eq!(
            named(
                "execute",
                "/usr/bin/bash -lc 'cat crates/a.rs\ncat crates/b.rs'"
            ),
            pair("cat", "crates/a.rs\ncat crates/b.rs")
        );
        assert_eq!(
            named("execute", "./scripts/check.sh --all"),
            pair("check.sh", "--all")
        );
        assert_eq!(named("execute", "cd crates"), pair("cd", "crates"));
        assert_eq!(named("execute", "Terminal"), pair("Terminal", ""));
    }

    #[test]
    fn other_calls_are_named_by_their_kind() {
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            named("Read", "Read file '/home/me/a.rs'"),
            pair("read", "file '/home/me/a.rs'")
        );
        assert_eq!(
            named("search", "grep -n foo src"),
            pair("search", "grep -n foo src")
        );
        assert_eq!(
            named("other", "mcp__docs__batch {…}"),
            pair("mcp__docs__batch", "{…}")
        );
    }

    fn item(id: &str, turn: &str, tool: Option<Tool>) -> Item {
        Item {
            item_id: id.into(),
            created_seq: "1".into(),
            turn_id: Some(turn.into()),
            submission_id: None,
            agent_generation_id: "g".into(),
            kind: if tool.is_some() { "tool" } else { "agent" }.into(),
            content: vec![],
            tool,
        }
    }

    #[test]
    fn a_run_is_consecutive_calls_in_one_turn() {
        let call = |id: &str, turn: &str| item(id, turn, Some(tool("execute", "ls")));
        let items = [
            call("a", "t1"),
            call("b", "t1"),
            item("text", "t1", None),
            call("c", "t1"),
            call("asked", "t1"),
            call("d", "t1"),
            call("e", "t2"),
            call("f", "t2"),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let entries = entries(&refs, |item| item.item_id == "asked");
        assert_eq!(entries, vec![0..2, 2..3, 3..4, 4..5, 5..6, 6..8]);
    }

    #[test]
    fn groups_keep_the_order_their_first_call_came_in() {
        let call = |id: &str, title: &str| item(id, "t", Some(tool("execute", title)));
        let items = [
            call("1", "sed -n 1,2p a"),
            call("2", "grep x a"),
            call("3", "sed -n 3,4p b"),
            item("4", "t", Some(tool("read", "Read b"))),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let groups = groups(&refs);
        let shape: Vec<(&str, usize)> = groups
            .iter()
            .map(|g| (g.name.as_str(), g.members.len()))
            .collect();
        assert_eq!(shape, vec![("sed", 2), ("grep", 1), ("read", 1)]);
        assert_eq!(groups[0].latest(), "-n 3,4p b");
    }

    #[test]
    fn a_group_shows_running_before_failed_before_waiting() {
        let with = |statuses: &[&str]| {
            let items: Vec<Item> = statuses
                .iter()
                .enumerate()
                .map(|(i, status)| {
                    let mut t = tool("execute", "ls");
                    t.status = (*status).into();
                    item(&i.to_string(), "t", Some(t))
                })
                .collect();
            let refs: Vec<&Item> = items.iter().collect();
            groups(&refs)[0].status()
        };
        assert_eq!(with(&["completed", "failed", "in_progress"]), "in_progress");
        assert_eq!(with(&["Completed", "Failed", "pending"]), "failed");
        assert_eq!(with(&["completed", "pending"]), "pending");
        assert_eq!(with(&["Completed", "completed"]), "completed");
    }
}
