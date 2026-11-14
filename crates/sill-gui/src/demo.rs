//! Synthetic content only; no private transcript is embedded in the binary. A turn as Sill shows
//! one: a prompt, a finished shell call, agent text streaming in (`CHUNKS`), then an edit that
//! waits on a permission question (`waiting`), answered by `answered`. Everything arrives as SFP
//! events, so the demo drives the same model as a live session.
use crate::model::Conversation;
use sill_proto::sfp::*;

pub const CHUNKS: &[&str] = &[
    "A streamed paragraph with **bold**, *italic*, and `inline code` — the send arrow is drawn in `conversation.rs`.\n\n",
    "| Check | Result |\n| --- | --- |\n| Markdown | Rendered |\n| Unicode | café · 日本語 · 🦀 |\n\n",
    "```rust\nfn main() {\n    println!(\"Sill GUI\");\n}\n```\n\n",
    "- Select text within a paragraph, then press Ctrl+C.\n- Code blocks have a copy button while the pointer is over them.\n\n",
    "I'll make the arrow `1em` instead of `1.1em`; the button size stays the same.\n",
];

/// Wall-clock milliseconds, for the demo's working interval.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn session(activity: &str, question: Option<&str>) -> SessionState {
    let working = activity == "working";
    serde_json::from_value(serde_json::json!({
        "session_id":"demo", "agent_generation_id":"demo-generation", "lifecycle":"running",
        "title":"Iced conversation preview", "deleted":false, "activity":activity,
        "active_ms": 4000, "active_since_ms": if working { Some(now_ms()) } else { None },
        "turn_id": if activity == "idle" { None } else { Some("demo-turn") },
        "question_ids": question.into_iter().collect::<Vec<_>>(), "capabilities":{},
        "usage": {"used":"23000", "size":"200000"},
        "controls": {
            "current_mode": "default",
            "modes": [{"value":"default","title":"Default"}, {"value":"plan","title":"Plan"}],
            "options": [
                {"option_id":"model", "title":"Model", "category":"model",
                 "value":{"type":"id","value":"astra"},
                 "choices":[{"value":"astra","title":"6 Astra"}, {"value":"brisk","title":"6 Brisk"}]},
                {"option_id":"effort", "title":"Effort", "category":"thought_level",
                 "value":{"type":"id","value":"medium"},
                 "choices":[{"value":"low","title":"Low"}, {"value":"medium","title":"Medium"}, {"value":"high","title":"High"}]},
                {"option_id":"web", "title":"Web search", "category":"other",
                 "value":{"type":"boolean","value":false}, "choices":[]}
            ]
        }
    }))
    .expect("valid synthetic session")
}

fn item(id: &str, seq: &str, kind: &str, text: &str) -> Item {
    Item {
        item_id: id.into(),
        created_seq: seq.into(),
        turn_id: Some("demo-turn".into()),
        submission_id: None,
        agent_generation_id: "demo-generation".into(),
        kind: kind.into(),
        content: if text.is_empty() {
            vec![]
        } else {
            vec![Content::text(text)]
        },
        tool: None,
    }
}

fn tool(title: &str, kind: &str, status: &str, detail: &str, diff: &str) -> Tool {
    Tool {
        title: title.into(),
        kind: kind.into(),
        status: status.into(),
        detail: detail.into(),
        diff: diff.into(),
        parent_item_id: None,
    }
}

pub fn conversation() -> Conversation {
    let mut view = Conversation {
        session_id: "demo".into(),
        subscription_id: "demo-subscription".into(),
        seq: 3,
        session: Some(session("working", None)),
        ..Conversation::default()
    };
    view.items.insert(
        "user".into(),
        item(
            "user",
            "1",
            "user",
            "Make the send arrow a little smaller and show me the change.",
        ),
    );
    let mut shell = item("tool", "2", "tool", "");
    shell.tool = Some(tool(
        "rg -n 'sill-send' crates/sill/src/shelf",
        "execute",
        "completed",
        "crates/sill/src/shelf/conversation.rs:1640:button.sill-send > image { -gtk-icon-size: 1.1em; }",
        "",
    ));
    view.items.insert("tool".into(), shell);
    view.items.insert(
        "agent".into(),
        item(
            "agent",
            "3",
            "agent",
            "The arrow's size is one rule in the stylesheet.\n\n",
        ),
    );
    view
}

fn event(seq: u64, event: Event) -> SessionEvent {
    SessionEvent {
        session_id: "demo".into(),
        subscription_id: "demo-subscription".into(),
        seq: seq.to_string(),
        event,
    }
}

pub fn append(seq: u64, text: &str) -> SessionEvent {
    event(
        seq,
        Event::ItemTextAppended {
            item_id: "agent".into(),
            block_id: "text".into(),
            text: text.into(),
        },
    )
}

fn question(state: &str, answer: Option<&str>) -> Question {
    serde_json::from_value(serde_json::json!({
        "question_id":"demo-question", "agent_generation_id":"demo-generation",
        "kind":"permission", "title":"Edit conversation.rs",
        "tool_item_id":"edit",
        "options":[
            {"option_id":"allow", "title":"Yes", "kind":"allow_once"},
            {"option_id":"allow-session", "title":"Yes, allow all edits during this session", "kind":"allow_always"},
            {"option_id":"reject", "title":"No", "kind":"reject_once"}],
        "fields":[], "state":state,
        "answer": answer.map(|option| serde_json::json!({"type":"permission", "option_id":option}))
    }))
    .expect("valid synthetic question")
}

const DIFF: &str = "@@ -1638,3 +1638,3 @@\n button.sill-send { min-width: 1.6em; }\n-button.sill-send > image { -gtk-icon-size: 1.1em; }\n+button.sill-send > image { -gtk-icon-size: 1em; }\n";

/// After the last chunk: the edit, its question, the session waiting on it.
pub fn waiting(seq: u64) -> Vec<SessionEvent> {
    let mut edit = item("edit", &(seq).to_string(), "tool", "");
    edit.tool = Some(tool("Edit conversation.rs", "edit", "pending", "", DIFF));
    vec![
        event(seq, Event::ItemCreated { item: edit }),
        event(
            seq + 1,
            Event::QuestionUpdated {
                question: question("open", None),
            },
        ),
        event(
            seq + 2,
            Event::SessionUpdated {
                session: session("waiting", Some("demo-question")),
            },
        ),
    ]
}

/// The person answered: the question resolves, the edit runs, the agent closes the turn.
pub fn answered(seq: u64, option: &str) -> Vec<SessionEvent> {
    let allowed = option != "reject";
    let mut events = vec![
        event(
            seq,
            Event::QuestionUpdated {
                question: question("answered", Some(option)),
            },
        ),
        event(
            seq + 1,
            Event::ItemPatched {
                item_id: "edit".into(),
                set: ItemPatch {
                    content: None,
                    tool: Some(tool(
                        "Edit conversation.rs",
                        "edit",
                        if allowed { "completed" } else { "failed" },
                        "",
                        DIFF,
                    )),
                },
            },
        ),
    ];
    let reply = if allowed {
        "Done: the send arrow is now `1em`; the button keeps its size."
    } else {
        "Understood — I left `conversation.rs` as it was."
    };
    events.push(event(
        seq + 2,
        Event::ItemCreated {
            item: item("reply", &(seq + 2).to_string(), "agent", reply),
        },
    ));
    events.push(event(
        seq + 3,
        Event::SessionUpdated {
            session: session("idle", None),
        },
    ));
    events
}

/// A prompt typed into the demo's composer: shown, then a note that no agent answers it.
pub fn prompt(seq: u64, text: &str) -> Vec<SessionEvent> {
    let id = format!("prompt-{seq}");
    vec![
        event(
            seq,
            Event::ItemCreated {
                item: item(&id, &seq.to_string(), "user", text),
            },
        ),
        event(
            seq + 1,
            Event::ItemCreated {
                item: item(
                    &format!("{id}-note"),
                    &(seq + 1).to_string(),
                    "note",
                    "The demo has no agent: nothing answers this prompt.",
                ),
            },
        ),
    ]
}
