//! Turns sill started for the person — a summary it asked the agent for, the commit Apply asked
//! for — folded to one line that opens to them (docs/sill-gui.md, *Folded turns*). What a turn is
//! for is the protocol's (`Turn::purpose`, `Submission::purpose`), never read from its words.
//! A fold holds Apply's prompt with the turn it started. An item with a question under it keeps
//! its own row, so a question is never hidden in a fold. Apply commit replies stay visible too.
use std::ops::Range;

use sill_proto::sfp::Item;

const FIX: &str = sill_proto::crosscheck::FIX_PURPOSE;

/// One row of the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// One item, or a run of tool calls when it holds more than one (`tool_runs`).
    Items(Range<usize>),
    /// A folded turn for `purpose`.
    Fold(Range<usize>, String),
}

impl Row {
    pub fn range(&self) -> &Range<usize> {
        match self {
            Row::Items(range) | Row::Fold(range, _) => range,
        }
    }
}

/// The feed's rows over `items`. `purpose` is an item's (`Conversation::purpose`);
/// `stands_alone` names items that keep their own row.
pub fn rows<'a>(
    items: &[&'a Item],
    purpose: impl Fn(&'a Item) -> Option<&'a str>,
    stands_alone: impl Fn(&Item) -> bool,
) -> Vec<Row> {
    // of a Crosscheck fix request only the prompt folds: the turn it starts is the agent's work
    let folded = |item: &'a Item| {
        purpose(item).filter(|why| !stands_alone(item) && !(*why == "commit" && item.kind == "agent") && !(*why == FIX && item.kind != "user"))
    };
    let mut rows = Vec::new();
    let mut plain = 0;
    let mut index = 0;
    while index < items.len() {
        let Some(why) = folded(items[index]) else {
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        // the turn's items after it; after a prompt, the turn that prompt started
        while index < items.len() {
            let (last, item) = (items[index - 1], items[index]);
            let same = if last.kind == "user" {
                item.kind != "user" && item.turn_id.is_some()
            } else {
                item.turn_id.is_some() && item.turn_id == last.turn_id
            };
            if !same || folded(item) != Some(why) {
                break;
            }
            index += 1;
        }
        push_plain(&mut rows, items, plain..start, &stands_alone);
        rows.push(Row::Fold(start..index, why.to_string()));
        plain = index;
    }
    push_plain(&mut rows, items, plain..items.len(), &stands_alone);
    rows
}

fn push_plain(rows: &mut Vec<Row>, items: &[&Item], range: Range<usize>, stands_alone: &impl Fn(&Item) -> bool) {
    let offset = range.start;
    rows.extend(
        crate::tool_runs::entries(&items[range], stands_alone)
            .into_iter()
            .map(|r| Row::Items(r.start + offset..r.end + offset)),
    );
}

/// What a fold's line says when it is the folded prompt's own first line: a Crosscheck fix
/// request, which sill wrote and begins with what the person pressed for ("Fix the 2 problems
/// codex found"). Other folds say [`label`].
pub fn said(purpose: &str, first: &Item) -> Option<String> {
    if purpose != FIX || first.kind != "user" {
        return None;
    }
    let text = first.content.iter().find_map(|c| c.text.as_deref())?;
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// The fold's line: what sill asked for, and whether the agent is still at it.
pub fn label(purpose: &str, running: bool) -> &'static str {
    match (purpose, running) {
        ("summary", true) => "Writing the session's summary…",
        ("summary", false) => "Session summary",
        ("commit", true) => "Committing for Apply…",
        ("commit", false) => "Committed for Apply",
        (FIX, _) => "Asked to fix what Crosscheck found",
        (_, true) => "Working for sill…",
        (_, false) => "Done for sill",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str, turn: Option<&str>, tool: bool) -> Item {
        Item {
            item_id: id.into(),
            created_seq: "1".into(),
            turn_id: turn.map(Into::into),
            submission_id: None,
            agent_generation_id: "g".into(),
            kind: kind.into(),
            content: vec![],
            tool: tool.then(|| sill_proto::sfp::Tool {
                title: "read".into(),
                kind: "read".into(),
                status: "completed".into(),
                detail: String::new(),
                diff: String::new(),
                parent_item_id: None,
                command: None,
            }),
        }
    }

    #[test]
    fn a_purposed_prompt_folds_with_its_turn_and_nothing_else() {
        let items = [
            item("ask", "user", None, false),
            item("a", "agent", Some("t1"), false),
            item("apply", "user", None, false),
            item("c1", "tool", Some("t2"), true),
            item("c2", "tool", Some("t2"), true),
            item("c3", "agent", Some("t2"), false),
            item("note", "note", None, false),
            item("s1", "agent", Some("t3"), false),
            item("next", "user", None, false),
            item("r1", "tool", Some("t4"), true),
            item("r2", "tool", Some("t4"), true),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let purpose = |i: &Item| match i.item_id.as_str() {
            "apply" | "c1" | "c2" | "c3" => Some("commit"),
            "s1" => Some("summary"),
            _ => None,
        };
        let rows = rows(&refs, purpose, |_| false);
        assert_eq!(
            rows,
            vec![
                Row::Items(0..1),
                Row::Items(1..2),
                Row::Fold(2..5, "commit".into()),
                Row::Items(5..6),
                Row::Items(6..7),
                Row::Fold(7..8, "summary".into()),
                Row::Items(8..9),
                Row::Items(9..11),
            ]
        );
    }

    #[test]
    fn an_item_with_a_question_breaks_the_fold() {
        let items = [
            item("c1", "tool", Some("t"), true),
            item("c2", "tool", Some("t"), true),
            item("c3", "agent", Some("t"), false),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let rows = rows(&refs, |_| Some("commit"), |i| i.item_id == "c2");
        assert_eq!(
            rows,
            vec![
                Row::Fold(0..1, "commit".into()),
                Row::Items(1..2),
                Row::Items(2..3),
            ]
        );
    }

    #[test]
    fn of_a_crosscheck_fix_request_only_the_prompt_folds() {
        let mut prompt = item("p", "user", None, false);
        prompt.content = vec![serde_json::from_value(serde_json::json!({"block_id": "b", "type": "text", "text": "\nFix the 2 problems codex found\n\nA second opinion…"})).unwrap()];
        let items = [item("before", "agent", Some("t0"), false), prompt, item("work", "tool", Some("t1"), true), item("answer", "agent", Some("t1"), false)];
        let refs: Vec<&Item> = items.iter().collect();
        let purpose = |i: &Item| match i.item_id.as_str() {
            "p" | "work" | "answer" => Some(FIX),
            _ => None,
        };
        let rows = rows(&refs, purpose, |_| false);
        assert_eq!(rows, [Row::Items(0..1), Row::Fold(1..2, FIX.to_string()), Row::Items(2..3), Row::Items(3..4)]);
        assert_eq!(said(FIX, refs[1]).as_deref(), Some("Fix the 2 problems codex found"));
        assert_eq!(said("commit", refs[1]), None);
        assert_eq!(said(FIX, refs[2]), None);
    }
}
