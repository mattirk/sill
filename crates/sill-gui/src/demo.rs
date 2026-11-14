//! Synthetic content only; no private transcript is embedded in the binary. A turn as Sill shows
//! one: a prompt, a finished shell call, agent text streaming in (`CHUNKS`), then an edit that
//! waits on a permission question (`waiting`), answered by `answered`. Everything arrives as SFP
//! events, so the demo drives the same model as a live session.
use sill_client::Conversation;
use sill_client::files::{Data, Request, Root, name};
use sill_proto::sessions::HostPath;
use sill_proto::sfp::*;
use sill_proto::{admin, discovery};

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
    let mut view = Conversation::new("demo", "demo-subscription", 3);
    view.session = Some(session("working", None));
    view.insert_item(item(
        "user",
        "1",
        "user",
        "Make the send arrow a little smaller and show me the change.",
    ));
    let mut shell = item("tool", "2", "tool", "");
    shell.tool = Some(tool(
        "rg -n 'SEND_SIZE' crates/sill-gui/src",
        "execute",
        "completed",
        "crates/sill-gui/src/icons.rs:18:const SEND_SIZE: f32 = 1.1;",
        "",
    ));
    view.insert_item(shell);
    view.insert_item(item(
        "agent",
        "3",
        "agent",
        "The arrow's size is one rule in the stylesheet.\n\n",
    ));
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

const DIFF: &str = "@@ -17,3 +17,3 @@\n /// The send arrow's size, relative to the composer's text.\n-const SEND_SIZE: f32 = 1.1;\n+const SEND_SIZE: f32 = 1.0;\n";

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

fn path(path: &str) -> HostPath {
    HostPath::from_path(std::path::Path::new(path))
}

fn metadata(directory: bool) -> admin::FileMetadata {
    admin::FileMetadata {
        file: !directory,
        directory,
        symlink: false,
        target: None,
        length: "150".into(),
        readonly: false,
    }
}

/// The demo's files: one pinned project with a few folders, a Markdown file and a diff.
pub fn files(request: &Request) -> Result<Data, String> {
    Ok(match request {
        Request::Roots => Data::Roots {
            roots: vec![Root {
                path: path("/home/demo/project"),
                global: false,
                metadata: Some(metadata(true)),
                error: None,
            }],
            home: path("/home/demo"),
        },
        Request::Directory { path: dir, .. } => {
            let entries = match dir.display.as_str() {
                "/home/demo" => vec![("project", true)],
                "/home/demo/project" => vec![
                    ("src", true),
                    ("docs", true),
                    ("README.md", false),
                    ("Cargo.toml", false),
                    (".gitignore", false),
                ],
                "/home/demo/project/src" => vec![("main.rs", false)],
                "/home/demo/project/docs" => vec![("guide.md", false)],
                _ => vec![],
            };
            Data::Directory(admin::FilesPage {
                entries: entries
                    .into_iter()
                    .map(|(entry, directory)| admin::FileEntry {
                        path: path(&format!("{}/{entry}", dir.display)),
                        metadata: metadata(directory),
                    })
                    .collect(),
                next: None,
            })
        }
        Request::Inspect(at) => Data::Metadata(metadata(!name(at).contains('.'))),
        Request::File {
            path: file,
            repo: Some(_),
        } => Data::File(format!(
            "diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n@@ -1,3 +1,4 @@\n # Sill\n-A conversation preview.\n+A place to work with your agents.\n+Files and diffs stay with each session.\n",
            name(file)
        )),
        Request::File { path: file, .. } => Data::File(if file.display.ends_with(".rs") {
            "fn main() {\n    println!(\"Hello from Sill\");\n}\n".into()
        } else {
            "# Your session's files\n\nBrowse pinned folders, read files and review **what changed** without leaving the conversation.\n\n## Staying oriented\n\n- Expanded folders stay open across refreshes.\n- Each session remembers its own files.\n- Close the viewer to return to the conversation.\n\n```rust\nfn main() {\n    println!(\"Hello from Sill\");\n}\n```\n\nThis is a synthetic file in the demo.\n".into()
        }),
        Request::Repo { path: repo, counts } => Data::Repo {
            root: Some(repo.clone()),
            head: "main".into(),
            entries: vec![
                discovery::StatusEntry {
                    path: path("/home/demo/project/README.md"),
                    status: "M".into(),
                },
                discovery::StatusEntry {
                    path: path("/home/demo/project/src/main.rs"),
                    status: "?".into(),
                },
            ],
            summary: "+12 −3  ?1".into(),
            lines: Some(("12".into(), "3".into())),
            counts: if *counts {
                vec![
                    (path("/home/demo/project/README.md"), Some((12, 3))),
                    (path("/home/demo/project/src/main.rs"), Some((3, 0))),
                ]
            } else {
                vec![]
            },
            truncated: false,
        },
        Request::Pin { .. } => {
            return Err("Demo pins are examples; use a real session to change pins.".into());
        }
    })
}

/// The demo's Changes page: the pinned project's own changes, the session's writes outside it
/// (one changed on the host meanwhile), and an earlier keep that can be undone.
pub fn changes() -> Vec<sill_proto::changes::Entry> {
    let written = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        - 25 * 60)
        .to_string();
    let overlay = |file: &str, kind: &str, conflict: bool| {
        serde_json::json!({"type":"overlay", "session_id":"demo", "title":"witty-willow",
            "ghost": false, "agent_state": false, "path": path(file), "kind": kind,
            "file_type":"file", "size":"120", "conflict": conflict, "written": written,
            "repo": null, "hash":"demo-hash", "protected": null})
    };
    [
        serde_json::json!({"type":"live", "repo": path("/home/demo/project"),
            "path": path("/home/demo/project/README.md"), "status":"M"}),
        serde_json::json!({"type":"live", "repo": path("/home/demo/project"),
            "path": path("/home/demo/project/notes.txt"), "status":"?"}),
        overlay("/home/demo/.config/app/settings.toml", "modified", false),
        overlay("/home/demo/scratch/plan.md", "added", false),
        overlay("/home/demo/.profile", "modified", true),
        serde_json::json!({"type":"kept", "session_id":"demo", "title":"witty-willow",
            "ghost": false, "keep_id":"k1", "timestamp":"1793806135",
            "path": path("/home/demo/.bashrc"), "what":"wrote", "before":"a file", "hash": null}),
    ]
    .into_iter()
    .map(|entry| serde_json::from_value(entry).expect("a demo change"))
    .collect()
}

pub fn changes_diff() -> String {
    "--- a/settings.toml\n+++ b/settings.toml\n@@ -1,3 +1,3 @@\n [ui]\n-theme = \"light\"\n+theme = \"dracula\"\n size = 13\n".into()
}
