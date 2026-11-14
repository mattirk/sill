//! Synthetic content only; no private transcript is embedded in the binary.
use crate::model::Conversation;
use sill_proto::sfp::*;

pub const CHUNKS: &[&str] = &[
    "A streamed paragraph with **bold**, *italic*, and `inline code`.\n\n",
    "| Check | Result |\n| --- | --- |\n| Markdown | Rendered |\n| Unicode | café · 日本語 · 🦀 |\n\n",
    "```rust\nfn main() {\n    println!(\"Sill GUI\");\n}\n```\n\n",
    "- Select text within a paragraph, then press Ctrl+C.\n- Use Copy for a whole message.\n\n",
    "> Select transcript text opens a read-only text snapshot for selection across messages.\n\n",
    "[A link](https://iced.rs) copies its address when clicked in this preview.\n",
];

pub fn conversation() -> Conversation {
    let session: SessionState = serde_json::from_value(serde_json::json!({
        "session_id":"demo", "agent_generation_id":"demo-generation", "lifecycle":"running",
        "title":"Iced conversation preview", "deleted":false, "activity":"waiting",
        "question_ids":["demo-question"], "capabilities":{}
    }))
    .expect("valid synthetic session");
    let mut view = Conversation {
        session_id: "demo".into(),
        subscription_id: "demo-subscription".into(),
        seq: 3,
        session: Some(session),
        ..Conversation::default()
    };
    for (id, seq, kind, text) in [
        (
            "user",
            "1",
            "user",
            "Show a representative conversation with Markdown and streaming.",
        ),
        ("tool", "2", "tool", "Checked the example workspace."),
        ("agent", "3", "agent", "## A conversation in Iced\n\n"),
    ] {
        view.items.insert(
            id.into(),
            Item {
                item_id: id.into(),
                created_seq: seq.into(),
                turn_id: None,
                submission_id: None,
                agent_generation_id: "demo-generation".into(),
                kind: kind.into(),
                content: vec![Content::text(text)],
                tool: None,
            },
        );
    }
    view.items.get_mut("tool").unwrap().tool = Some(Tool {
        title: "Read workspace".into(),
        kind: "read".into(),
        status: "completed".into(),
        detail: String::new(),
        diff: String::new(),
        parent_item_id: None,
    });
    let question: Question = serde_json::from_value(serde_json::json!({
        "question_id":"demo-question", "agent_generation_id":"demo-generation",
        "kind":"permission", "title":"Allow the example command?",
        "description":"This is a synthetic question card; no command is executed.",
        "options":[{"option_id":"allow", "title":"Allow once", "kind":"allow_once"},
                   {"option_id":"deny", "title":"Reject", "kind":"reject_once"}],
        "fields":[], "state":"open"
    }))
    .expect("valid synthetic question");
    view.questions
        .insert(question.question_id.clone(), question);
    view
}

pub fn append(seq: u64, text: &str) -> SessionEvent {
    SessionEvent {
        session_id: "demo".into(),
        subscription_id: "demo-subscription".into(),
        seq: seq.to_string(),
        event: Event::ItemTextAppended {
            item_id: "agent".into(),
            block_id: "text".into(),
            text: text.into(),
        },
    }
}
