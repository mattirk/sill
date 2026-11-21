//! Consecutive tool calls shown as one block, grouped by what they run (docs/silli-gui.md, *Tool
//! runs*). A run is an unbroken sequence of tool calls in one turn: anything else — the agent's
//! text, a thought, the person's prompt, a question under a call — ends it, so nothing moves past
//! what the agent said or was asked. Within a run the calls are grouped by the program their
//! command runs (`sed`, `grep`) or, for calls that declare no command, by their kind (`read`,
//! `edit`); the groups are in the order their first call came, so a new call never moves a group
//! already shown. A command is what the agent declared the call runs (`Tool::command`), never
//! what its title looks like.
use std::ops::Range;

use silli_proto::sfp::{Item, Tool};

use crate::shell::Step;

/// The feed's rows over `items`: each range one row, a run of tool calls when it holds more than
/// one item. `stands_alone` names items that must keep their own row (a call with a question
/// under it).
///
/// Consecutive Host control notices make a row of their own the same way (`host_runs`), whatever
/// their turn: never joined with tool calls (the person, 2026-11-13).
pub fn entries(items: &[&Item], stands_alone: impl Fn(&Item) -> bool) -> Vec<Range<usize>> {
    let joins = |item: &Item| item.tool.is_some() && !stands_alone(item);
    let host = |item: &Item| crate::host_runs::host_text(item).is_some();
    let mut entries: Vec<Range<usize>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let previous = entries.last().map(|last| items[last.end - 1]);
        let together = previous.is_some_and(|previous| (joins(item) && joins(previous) && previous.turn_id == item.turn_id) || (host(item) && host(previous)));
        match entries.last_mut() {
            Some(last) if together => last.end = index + 1,
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
        let has = |wanted: &str| self.members.iter().any(|(_, tool)| normal_status(&tool.status) == wanted);
        if has("inprogress") {
            "in_progress"
        } else if has("failed") {
            "failed"
        } else if self.members.iter().any(|(_, tool)| normal_status(&tool.status) != "completed") {
            "pending"
        } else {
            "completed"
        }
    }

    /// The latest call's title after the group's name: what it ran this time.
    pub fn latest(&self) -> String {
        self.members.last().map(|(_, tool)| classify(tool).1).unwrap_or_default()
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
        match groups.iter_mut().find(|g| g.name == name && g.kind.eq_ignore_ascii_case(kind)) {
            Some(group) => group.members.push((item, tool)),
            None => groups.push(Group { name, kind, members: vec![(item, tool)] }),
        }
    }
    groups
}

/// A shell call's command: what the agent declared it runs (`Tool::command`, which the daemon
/// fills from the call's input for calls of kind `execute`). Never the title: that is display
/// text, and an agent may word it as it likes.
fn shell(tool: &Tool) -> Option<&str> {
    tool.command.as_deref().map(str::trim).filter(|command| !command.is_empty())
}

/// What a call's row shows, line by line. A shell call that ran several commands has one line
/// for each, with the operator that ends it (`crate::shell`); every other call has one line:
/// its command whole, or its title.
pub fn lines(tool: &Tool) -> Vec<Step<'_>> {
    let whole = |text| vec![Step { text, operator: "" }];
    match shell(tool) {
        Some(command) => match crate::shell::steps(command) {
            Some(steps) if steps.len() > 1 => steps,
            _ => whole(command),
        },
        None => whole(tool.title.trim()),
    }
}

/// A call's name, and the text that goes beside the name. A shell call is named by the program
/// its command runs ([`program`]), and has no name when it declared no command: the name column
/// holds programs, and nothing says which one a title stands for. Every other call is named by
/// its kind (`read`, `search`), or by its title's first word when the kind says nothing
/// (`other`).
pub fn classify(tool: &Tool) -> (String, String) {
    let kind = tool.kind.to_ascii_lowercase();
    if let Some(command) = shell(tool) {
        return program(command).unwrap_or((String::new(), command.to_owned()));
    }
    let title = tool.title.trim();
    match kind.as_str() {
        "execute" => (String::new(), title.to_owned()),
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

/// The program a command line runs, and the text beside it. The program is that of the first
/// command that runs one: past `cd …` steps, comments and lone assignments, past the
/// assignments before a command, inside a shell's `-c` script, as the program's file name.
/// The text is the command without its name only when it starts with exactly that name
/// (`sed -n 1p a` beside `sed` is `-n 1p a`); otherwise it is the whole command, so the name
/// and the text together always hold all of it.
fn program(command: &str) -> Option<(String, String)> {
    let whole = [Step { text: command.trim(), operator: "" }];
    let steps = crate::shell::steps(command);
    let steps = steps.as_deref().unwrap_or(&whole);
    steps.iter().enumerate().find_map(|(index, step)| {
        if step.text.starts_with('#') {
            return None;
        }
        let mut rest = step.text;
        let (word, after) = loop {
            let (word, after) = first_word(rest)?;
            if !is_assignment(word) {
                break (word, after);
            }
            rest = after;
        };
        if word == "cd" && index + 1 < steps.len() {
            return None;
        }
        let name = word.rsplit('/').next().unwrap_or(word);
        if matches!(name, "sh" | "bash" | "zsh" | "dash")
            && let Some((flags, script)) = first_word(after)
            && flags.starts_with('-')
            && flags.contains('c')
        {
            let script = script.trim();
            let script = match script.chars().next() {
                Some(quote @ ('\'' | '"')) => unquoted(script, quote),
                _ => script,
            };
            if let Some((inner, _)) = program(script) {
                return Some((inner, step.text.to_owned()));
            }
        }
        let plain = name == word && std::ptr::eq(rest, step.text);
        let beside = if plain { after.trim() } else { step.text };
        Some((name.to_owned(), beside.to_owned()))
    })
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
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
            command: None,
        }
    }

    /// A shell call as Claude's and Codex's adapters report one: the command declared, and the
    /// same text as the title.
    fn shell_call(command: &str) -> Tool {
        Tool { command: Some(command.into()), ..tool("execute", command) }
    }

    fn pair(name: &str, beside: &str) -> (String, String) {
        (name.to_owned(), beside.to_owned())
    }

    #[test]
    fn a_command_is_named_by_the_program_it_runs() {
        let named = |command: &str| classify(&shell_call(command));
        assert_eq!(named("sed -n 960,1257p crates/x.rs"), pair("sed", "-n 960,1257p crates/x.rs"));
        assert_eq!(named("git status"), pair("git", "status"));
        assert_eq!(named("cd crates"), pair("cd", "crates"));
        assert_eq!(named("ls"), pair("ls", ""));
        // past the steps that only lead to it
        assert_eq!(named("cd /home/me/GIT/silli && grep -rn foo crates"), pair("grep", "-rn foo crates"));
        assert_eq!(named("# look first\nls -la"), pair("ls", "-la"));
        assert_eq!(named("X=1; make"), pair("make", ""));
        // the first of several
        assert_eq!(named("sed -n 1,2p a && grep x b; cat c"), pair("sed", "-n 1,2p a"));
        // a loop is not divided, and is named by what it starts with
        assert_eq!(named("for f in a b; do cat $f; done"), pair("for", "f in a b; do cat $f; done"));
    }

    /// The name and the text beside it hold the whole command: the text loses the name only
    /// where the command starts with exactly that name.
    #[test]
    fn the_text_beside_a_name_is_the_whole_command_unless_it_starts_with_the_name() {
        let named = |command: &str| classify(&shell_call(command));
        assert_eq!(named("RUST_LOG=debug cargo test -p silli-gui"), pair("cargo", "RUST_LOG=debug cargo test -p silli-gui"));
        assert_eq!(named("./scripts/check.sh --all"), pair("check.sh", "./scripts/check.sh --all"));
        assert_eq!(named("/usr/bin/bash -lc 'cat crates/a.rs\ncat crates/b.rs'"), pair("cat", "/usr/bin/bash -lc 'cat crates/a.rs\ncat crates/b.rs'"));
        assert_eq!(
            named("/usr/bin/bash -lc \"rg -n 'a|b' docs | head\nsed -n 1p x\""),
            pair("rg", "/usr/bin/bash -lc \"rg -n 'a|b' docs | head\nsed -n 1p x\"")
        );
    }

    /// Only a declared command is read as one. A shell call without it — an agent that
    /// declares none, a call whose input still streams, a record from before the field — has
    /// no name and is shown by its title, whatever the title looks like.
    #[test]
    fn a_title_is_never_read_as_a_command() {
        assert_eq!(classify(&tool("execute", "sed -n 1,2p a && grep x b")), pair("", "sed -n 1,2p a && grep x b"));
        assert_eq!(classify(&tool("Execute", "Terminal")), pair("", "Terminal"));
        assert_eq!(lines(&tool("execute", "sed -n 1,2p a && grep x b")), vec![Step { text: "sed -n 1,2p a && grep x b", operator: "" }]);
        // a title that became a description changes nothing while the command is declared
        let described = Tool { title: "Show the first lines; then search".into(), ..shell_call("sed -n 1,2p a && grep x b") };
        assert_eq!(classify(&described), pair("sed", "-n 1,2p a"));
        assert_eq!(lines(&described).len(), 2);
    }

    #[test]
    fn other_calls_are_named_by_their_kind() {
        assert_eq!(classify(&tool("Read", "Read file '/home/me/a.rs'")), pair("read", "file '/home/me/a.rs'"));
        assert_eq!(classify(&tool("search", "grep -n foo src")), pair("search", "grep -n foo src"));
        assert_eq!(classify(&tool("other", "mcp__docs__batch {…}")), pair("mcp__docs__batch", "{…}"));
    }

    #[test]
    fn a_call_that_ran_several_commands_has_a_line_for_each() {
        let call = shell_call("git for-each-ref | head -20; echo \"--- remotes:\"; git remote -v");
        let shown: Vec<(&str, &str)> = lines(&call).iter().map(|step| (step.text, step.operator)).collect();
        assert_eq!(shown, vec![("git for-each-ref | head -20", ";"), ("echo \"--- remotes:\"", ";"), ("git remote -v", ""),]);
        // one command, and what is not divided, stay one line: the command whole
        for whole in ["sed -i 's/a/b/; s/c/d/' file", "ls;", "python3 - <<'PY'\nprint(1); print(2)\nPY", "/usr/bin/bash -lc 'cat a\ncat b'"] {
            assert_eq!(lines(&shell_call(whole)), vec![Step { text: whole, operator: "" }], "{whole}");
        }
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
        let call = |id: &str, turn: &str| item(id, turn, Some(shell_call("ls")));
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
        let call = |id: &str, command: &str| item(id, "t", Some(shell_call(command)));
        let items = [
            call("1", "sed -n 1,2p a"),
            call("2", "grep x a"),
            call("3", "sed -n 3,4p b"),
            item("4", "t", Some(tool("read", "Read b"))),
            item("5", "t", Some(tool("execute", "Terminal"))),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let groups = groups(&refs);
        let shape: Vec<(&str, usize)> = groups.iter().map(|g| (g.name.as_str(), g.members.len())).collect();
        assert_eq!(shape, vec![("sed", 2), ("grep", 1), ("read", 1), ("", 1)]);
        assert_eq!(groups[0].latest(), "-n 3,4p b");
    }

    #[test]
    fn a_group_shows_running_before_failed_before_waiting() {
        let with = |statuses: &[&str]| {
            let items: Vec<Item> = statuses
                .iter()
                .enumerate()
                .map(|(i, status)| {
                    let mut t = shell_call("ls");
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
