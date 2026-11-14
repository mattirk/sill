//! Renders views headlessly to PNG for visual review against `docs/reference-ui/`, at the window's
//! start size (720×860; the simulator draws at scale 2, the captures' 1440×1720). Ignored by
//! default: `cargo test -p sill-gui snapshots -- --ignored`, then look in
//! `target/sill-gui-snapshots/`. These are for looking at, not pixel assertions.
use std::path::PathBuf;

use iced::{Font, Settings};

use crate::{App, demo};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/sill-gui-snapshots")
}

pub fn settings() -> Settings {
    crate::fonts::install();
    Settings {
        default_font: Font::with_name(crate::fonts::SANS_FAMILY),
        ..Settings::default()
    }
}

/// Render `app`'s view as `name` (any earlier image of that name is replaced).
pub fn render(app: &App, name: &str) {
    render_size(app, name, (720.0, 860.0));
}

pub fn render_size(app: &App, name: &str, size: (f32, f32)) {
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{name}-"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut ui = iced_test::Simulator::with_size(settings(), size, app.view());
    let snapshot = ui.snapshot(&crate::style::theme()).unwrap();
    assert!(snapshot.matches_image(dir.join(name)).unwrap());
}

#[test]
#[ignore = "writes PNGs for review; run explicitly"]
fn render_views() {
    use crate::Message;
    let (mut app, _) = App::new(true, None);
    // streaming: three chunks in, the agent working
    for _ in 0..3 {
        let _ = app.update(Message::DemoTick);
    }
    render(&app, "1-streaming");
    // the rest of the text, then the edit waits on its question
    for _ in 3..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    render(&app, "2-question");
    let _ = app.update(Message::Answer {
        question_id: "demo-question".into(),
        option_id: "allow".into(),
    });
    let _ = app.update(Message::ToggleTool("edit".into()));
    let _ = app.update(Message::ToggleTool("tool".into()));
    render(&app, "3-answered");
    // the jail shell in the prompt's place: the pretend shell lists its files
    // the task carries the pane's events; dropping it would hang the shell up
    let _events = app.update(Message::ToggleTerminal);
    let pane = app
        .views
        .get_mut("demo")
        .unwrap()
        .terminal
        .as_mut()
        .unwrap();
    pane.accept(crate::terminal::Event::Opened);
    crate::tests::settle(pane, "project$");
    pane.input(b"ls\r".to_vec());
    crate::tests::settle(pane, "build.sh");
    pane.accept(crate::terminal::Event::Output);
    render(&app, "5-terminal");
    let (mut empty, _) = App::new(true, None);
    empty.views.clear();
    empty.tabs.clear();
    empty.selected = None;
    render(&empty, "4-empty");
}

#[test]
#[ignore = "size calibration against docs/reference-ui/02"]
fn calibrate() {
    use iced::widget::text;
    let line = "Reduced the send arrow from 1.1em to 1em. Button size stays the same.";
    for size in [15.0_f32, 15.3, 15.5] {
        let view: iced::Element<'_, ()> = iced::widget::container(text(line).size(size))
            .padding(10)
            .into();
        let name = format!("calibrate-{}", size.to_string().replace('.', "_"));
        let _ = std::fs::remove_file(dir().join(format!("{name}-tiny-skia.png")));
        let mut ui = iced_test::Simulator::with_size(settings(), (720.0, 40.0), view);
        let snapshot = ui.snapshot(&crate::style::theme()).unwrap();
        assert!(snapshot.matches_image(dir().join(name)).unwrap());
    }
}

#[test]
#[ignore = "writes file-workflow PNGs for review"]
fn render_files() {
    use crate::{Message, files::Action};
    use sill_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_path(std::path::Path::new(s));
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(
        crate::layout::Mode::Drawer,
    )));
    let _ = app.update(Message::Files(Action::Toggle(path(
        "/home/demo/project/src",
    ))));
    render_size(&app, "6-drawer", (1000.0, 860.0));
    render(&app, "7-drawer-small");
    let _ = app.update(Message::Files(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: None,
    }));
    render_size(&app, "8-file", (1000.0, 860.0));
    let _ = app.update(Message::Files(Action::DiffMode(true)));
    let _ = app.update(Message::Files(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: Some(path("/home/demo/project")),
    }));
    render_size(&app, "9-diff", (1000.0, 860.0));
    render_size(&app, "10-narrow-drawer", (480.0, 700.0));
    // The agent waits on a question while the file is open; no bottom status panel.
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    render_size(&app, "11-file-waiting", (1000.0, 860.0));
}

#[test]
#[ignore = "writes the menu and form PNGs for review"]
fn render_lifecycle() {
    use crate::Message;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Menu(crate::menu::Event::Toggle));
    app.all_sessions = (0..3)
        .map(|i| {
            let titles = [
                "Permission modes",
                "Drawer layout and flicker",
                "Compact prompt queue",
            ];
            let title = titles[i];
            serde_json::from_value(serde_json::json!({
                "session_id": format!("s{i}"), "agent": if i == 1 { "codex" } else { "claude" },
                "title": title,
                "title_kind": "", "created_at": "1", "last_used": "1", "ended_at": "1793806135",
                "policy": "default", "auto_allow": false, "memory_scope": "",
                "summary": "a summary", "summary_kind": "", "ghost": false, "running": false,
                "pins": [], "settings": {}, "follows": null, "compacted": "0"
            }))
            .unwrap()
        })
        .collect();
    render(&app, "12-menu");
    let _ = app.update(Message::Menu(crate::menu::Event::Toggle));
    let view = app.views.get_mut("demo").unwrap();
    let question: sill_proto::sfp::Question = serde_json::from_value(serde_json::json!({
        "question_id":"form", "agent_generation_id":"demo-generation", "kind":"form",
        "title":"Where should this deploy?", "options":[], "state":"open",
        "fields":[
            {"key":"env", "title":"Environment", "description":"The stage the release goes to.",
             "kind":"choice", "choices":[{"value":"stage","title":"Staging","description":""},
                                         {"value":"prod","title":"Production","description":""}]},
            {"key":"notify", "title":"Tell the team", "description":"", "kind":"boolean", "choices":[]},
            {"key":"why", "title":"Why now", "description":"", "kind":"text", "choices":[]}
        ]
    }))
    .unwrap();
    let event = sill_proto::sfp::SessionEvent {
        session_id: "demo".into(),
        subscription_id: "demo-subscription".into(),
        seq: (view.conversation.seq + 1).to_string(),
        event: sill_proto::sfp::Event::QuestionUpdated { question },
    };
    view.accept(event);
    let _ = app.update(Message::FormEdit(
        "form".into(),
        crate::forms::Edit::Pick {
            key: "env".into(),
            value: "stage".into(),
        },
    ));
    let _ = app.update(Message::Latest);
    render(&app, "13-form");
}

#[test]
#[ignore = "writes the Changes page PNG for review"]
fn render_changes() {
    use crate::Message;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::ToggleChanges);
    let _ = app.update(Message::Changes(crate::changes_ui::Event::Loaded(Ok((
        demo::changes(),
        0,
    )))));
    let key = {
        let entries = app.changes_listing.entries.as_ref().unwrap().as_ref().unwrap();
        sill_client::changes::entry_key(&entries[2])
    };
    let _ = app.update(Message::Changes(crate::changes_ui::Event::Diff(
        key.clone(),
    )));
    let _ = app.update(Message::Changes(crate::changes_ui::Event::DiffLoaded(
        key,
        Ok(demo::changes_diff()),
    )));
    render(&app, "14-changes");
}

#[test]
#[ignore = "writes thought PNGs for review"]
fn render_thoughts() {
    use sill_proto::sfp::{Content, Item};
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    let view = app.views.get_mut("demo").unwrap();
    view.conversation.questions.clear();
    // as Codex sends its reasoning summaries: bold titles after blank lines
    for (seq, id, text) in [
        (900, "t1", "\n\n**Checking entry icons**"),
        (
            901,
            "t2",
            "\n\n**Adding UI snapshot test**\n\n**Reviewing UI behavior**",
        ),
        (902, "t3", "The `render` path keeps *one* parse per item."),
    ] {
        view.conversation.insert_item(Item {
            item_id: id.into(),
            created_seq: seq.to_string(),
            turn_id: Some("demo-turn".into()),
            submission_id: None,
            agent_generation_id: "demo-generation".into(),
            kind: "thought".into(),
            content: vec![Content::text(text)],
            tool: None,
        });
        view.render(id);
    }
    view.follow_tail = true;
    render(&app, "12-thoughts");
}

#[test]
#[ignore = "writes source and diff highlighting PNGs for review"]
fn render_syntax_views() {
    use crate::{Message, files::Action};
    use sill_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_path(std::path::Path::new(s));
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::Open {
        path: path("/home/demo/project/src/main.rs"),
        repo: None,
    }));
    render(&app, "syntax-source");
    let view = app.views.get_mut("demo").unwrap();
    let file = view.files.reader.as_mut().unwrap();
    file.repo = Some(path("/home/demo/project"));
    file.source = false;
    file.revision += 1;
    file.text = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    println!(\"old\");\n+    let café = 42;\n+    println!(\"Hello {café}\");\n }\n".into();
    view.sync_reader();
    render(&app, "syntax-diff");
    let before = crate::style::theme_id().to_owned();
    crate::style::set_theme("adwaita-light");
    render(&app, "syntax-light-diff");
    crate::style::set_theme(&before);
}

#[test]
#[ignore = "writes source editing and unsaved notice PNGs for review"]
fn render_source_editing() {
    use crate::{Message, files::Action};
    use iced::widget::text_editor;
    use sill_proto::sessions::HostPath;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::Open {
        path: HostPath::from_path(std::path::Path::new("/home/demo/project/src/main.rs")),
        repo: None,
    }));
    let _ = app.update(Message::Files(Action::Text(text_editor::Action::Edit(
        text_editor::Edit::Insert('X'),
    ))));
    render(&app, "edit-source");
    let _ = app.update(Message::Files(Action::CloseViewer));
    render(&app, "unsaved-source");
    render_size(&app, "narrow-unsaved-source", (480.0, 860.0));
}

#[test]
#[ignore = "writes timer boundary PNGs for visual review"]
fn render_timer_boundaries() {
    let (mut app, _) = App::new(true, None);
    app.activity_hovered = true;
    for (seconds, name) in [
        (599u64, "timer-09m59"),
        (600, "timer-10m00"),
        (1200, "timer-20m00"),
        (3599, "timer-59m59"),
        (3600, "timer-01h00"),
        (36000, "timer-10h00"),
        (360000, "timer-100h00"),
    ] {
        let state = app
            .views
            .get_mut("demo")
            .unwrap()
            .conversation
            .session
            .as_mut()
            .unwrap();
        state.active_ms = seconds * 1000;
        state.active_since_ms = None;
        state.activity = "idle".into();
        render(&app, name);
    }
}

#[test]
#[ignore = "writes login card PNGs for review"]
fn render_login() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    let view = app.views.get_mut("demo").unwrap();
    view.conversation.questions.clear();
    view.conversation.session.as_mut().unwrap().authentication = Some(
        serde_json::from_value(serde_json::json!({
            "message": "Authentication required",
            "methods": [
                {"method_id": "claude-ai-login", "title": "Claude Subscription",
                 "description": "Use Claude subscription", "terminal": {"args": [], "env": {}}},
                {"method_id": "console-login", "title": "Anthropic Console",
                 "description": "Use Anthropic Console (API usage billing)",
                 "terminal": {"args": [], "env": {}}},
                {"method_id": "key", "title": "API key", "description": null,
                 "terminal": null, "api_key": "anthropic"}
            ]
        }))
        .unwrap(),
    );
    view.follow_tail = true;
    render(&app, "13-login");
    let _ = app.update(crate::Message::Login(
        "demo".into(),
        crate::login_ui::Event::Pick("key".into()),
    ));
    render(&app, "14-login-key");
}

#[test]
#[ignore = "writes the demo conversation in Iceberg Dark and Light for review"]
fn render_iceberg() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    let before = crate::style::theme_id().to_owned();
    for id in ["iceberg-dark", "iceberg-light"] {
        assert!(crate::style::set_theme(id));
        render(&app, id);
    }
    crate::style::set_theme(&before);
}

#[test]
#[ignore = "writes the demo conversation in sill Dark and Light for review"]
fn render_sill() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    let before = crate::style::theme_id().to_owned();
    for id in ["sill-dark", "sill-light"] {
        assert!(crate::style::set_theme(id));
        render(&app, id);
    }
    crate::style::set_theme(&before);
}

#[test]
#[ignore = "writes network prompt PNGs for review"]
fn render_network() {
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    app.views
        .get_mut("demo")
        .unwrap()
        .conversation
        .questions
        .clear();
    let _ = app.update(crate::Message::NetworkRead(
        "demo".into(),
        Ok(serde_json::from_value(serde_json::json!({
            "next": null, "hosts": ["files.example.org:443"], "answered": [],
            "quiet": [], "agent_name": "Codex"
        }))
        .unwrap()),
    ));
    render(&app, "15-network");
}

#[test]
#[ignore = "writes managed-process PNGs for review"]
fn render_processes() {
    use crate::processes::Event;
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(crate::Message::DemoTick);
    }
    app.views
        .get_mut("demo")
        .unwrap()
        .conversation
        .questions
        .clear();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let list = serde_json::from_value(serde_json::json!([
        {"name": "web", "argv": ["npm", "run", "dev", "--", "--port", "5173"],
         "cwd": {"display": "/home/demo/project", "bytes_base64": "L2hvbWUvZGVtby9wcm9qZWN0"},
         "port": 5173, "pid": 41, "started": (now - 192).to_string(), "state": "running",
         "exit_code": null, "last_line": "  ➜  Local:   http://localhost:5173/"},
        {"name": "build", "argv": ["cargo", "build", "--release"],
         "cwd": {"display": "/home/demo/project", "bytes_base64": "L2hvbWUvZGVtby9wcm9qZWN0"},
         "port": null, "pid": 42, "started": "0", "state": "exited",
         "exit_code": 101, "last_line": "error: could not compile `demo` (bin \"demo\")"}
    ]))
    .unwrap();
    app.views
        .get_mut("demo")
        .unwrap()
        .processes
        .read(list, Some(&[]));
    render(&app, "16-process-question");
    let _ = app.update(crate::Message::Procs(Event::NotNow("demo".into(), 5173)));
    let _ = app.update(crate::Message::Procs(Event::Toggle("demo".into())));
    render(&app, "17-processes");
    let _ = app.update(crate::Message::Procs(Event::Log(
        "demo".into(),
        "build".into(),
    )));
    app.views.get_mut("demo").unwrap().processes.log = Some((
        "build".into(),
        iced::widget::text_editor::Content::with_text(
            "   Compiling demo v0.1.0\nerror[E0308]: mismatched types\n --> src/main.rs:3:5\nerror: could not compile `demo` (bin \"demo\")\n",
        ),
    ));
    render(&app, "18-process-log");
}

#[test]
#[ignore = "writes source gutter PNGs for review"]
fn render_source_gutter() {
    use crate::{Message, files::Action};
    use iced::widget::text_editor;
    use sill_proto::sessions::HostPath;
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Files(Action::Open {
        path: HostPath::from_path(std::path::Path::new("/home/demo/project/README.md")),
        repo: None,
    }));
    let _ = app.update(Message::Files(Action::Source));
    let content = (1..=130)
        .map(|line| {
            if line == 3 {
                format!("// {}", "wrapped source text ".repeat(18))
            } else if line == 4 {
                String::new()
            } else {
                format!("let value_{line} = {line};")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    app.views
        .get_mut("demo")
        .unwrap()
        .reader
        .as_mut()
        .unwrap()
        .content = crate::editor::Editor::with_text(&content);
    app.views
        .get_mut("demo")
        .unwrap()
        .reader
        .as_mut()
        .unwrap()
        .source_columns = crate::files::source_columns(&content);
    render_size(&app, "source-gutter-top", (720.0, 860.0));
    {
        let mut ui = iced_test::Simulator::with_size(settings(), (720.0, 860.0), app.view());
        ui.point_at(iced::Point::new(350.0, 250.0));
        let statuses = ui.simulate([iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
            delta: iced::mouse::ScrollDelta::Lines { x: -12.0, y: 0.0 },
        })]);
        assert_eq!(statuses, [iced::event::Status::Captured]);
        // Sideways scrolling is handled by the outer scrollbar, not a vertical editor action.
        let snapshot = ui.snapshot(&crate::style::theme()).unwrap();
        assert!(
            snapshot
                .matches_image(dir().join("source-horizontal-right"))
                .unwrap()
        );
        assert!(!ui.into_messages().any(|m| matches!(
            m,
            Message::Files(Action::Text(text_editor::Action::Scroll { .. }))
        )));
    }
    let _ = app.update(Message::Files(Action::Text(
        text_editor::Action::Scroll { lines: 95 },
    )));
    render_size(&app, "source-gutter-scrolled", (720.0, 860.0));
    let _ = app.update(Message::Files(Action::Text(text_editor::Action::Move(
        text_editor::Motion::DocumentEnd,
    ))));
    let _ = app.update(Message::Files(Action::Text(text_editor::Action::Edit(
        text_editor::Edit::Enter,
    ))));
    render_size(&app, "source-gutter-edited", (720.0, 860.0));
}

/// Workspaces (docs/parallel-coding.md §12): the pin chips' marks — a workspace and a repository
/// kept live with a warning — and the shelf above the prompt once the turn has ended, then with
/// its diff open and Discard armed.
#[test]
#[ignore = "writes the workspace shelf PNGs for review"]
fn render_workspaces() {
    use crate::Message;
    let (mut app, _) = App::new(true, None);
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let _ = app.update(Message::Answer { question_id: "demo-question".into(), option_id: "allow".into() });
    let host = |p: &str| sill_proto::sessions::HostPath::from_path(std::path::Path::new(p));
    let ws = serde_json::json!({"workspace_id": "ws-1", "name": "project", "session_id": "demo", "source": host("/home/demo/project"),
        "source_branch": "main", "base": "b0", "branch": "sill/project-ws-1", "path": host("/ws/project-ws-1"),
        "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"});
    let detail = serde_json::json!({"workspace": ws, "revision": {"tip": "t1", "worktree": "w", "destination_branch": "main", "destination_tip": "b0"},
        "summary": {"commits": 1, "files": 3, "added": "40", "removed": "12", "uncommitted": ["src/a.rs"]}, "needs_rebase": false});
    let view = app.views.get_mut("demo").unwrap();
    assert!(!view.conversation.busy(), "the turn has ended");
    view.workspaces.loaded = Some(sill_client::workspaces::Loaded {
        view: serde_json::from_value(serde_json::json!({"workspaces_mode": true, "workspaces": [ws],
            "live": [{"source": host("/home/demo/vendored"), "reason": "submodules", "message": "Live: Repositories with submodules are not supported yet."}]})).unwrap(),
        details: vec![serde_json::from_value(detail).unwrap()],
    });
    let dir = sill_proto::admin::FileMetadata { file: false, directory: true, symlink: false, target: None, length: "0".into(), readonly: false };
    view.files.roots.push(sill_client::files::Root { path: host("/home/demo/vendored"), global: false, metadata: Some(dir), error: None });
    render(&app, "workspaces-1-shelf");
    let view = app.views.get_mut("demo").unwrap();
    view.workspaces.diff = Some(("ws-1".into(), Ok("diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n fn main() {\n-    old();\n+    new();\n+    more();\n }\n".into())));
    view.workspaces.armed = Some("ws-1".into());
    render(&app, "workspaces-2-diff");
    // the person's checkout holds uncommitted work the workspace does not have: the chip's dot,
    // the drawer's line
    let view = app.views.get_mut("demo").unwrap();
    view.workspaces.diff = None;
    view.workspaces.armed = None;
    view.workspaces.loaded.as_mut().unwrap().details[0].checkout_only = 11;
    let _ = app.update(Message::Layout(crate::layout::Action::Mode(crate::layout::Mode::Drawer)));
    let _ = app.update(Message::Files(crate::files::Action::DiffMode(true)));
    render_size(&app, "workspaces-3-checkout-only", (1000.0, 500.0));
}

/// A run of shell calls between the demo's first call and its answer, as Claude makes them:
/// several commands to a call, joined by `;` and `&&`. Grouped by program (`tool_runs`), a call
/// that ran several commands shown as a line for each: folded, with one group opened, and with
/// a call in that group opened too.
#[test]
#[ignore = "writes tool-run PNGs for review"]
fn render_tool_runs() {
    use crate::Message;
    use sill_proto::sfp::{Item, Tool};
    let (mut app, _) = App::new(true, None);
    for _ in 0..demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    let calls = [
        ("sed -n 960,1257p crates/sill-core/src/workspace/records.rs", "completed"),
        ("cat crates/sill/src/daemon/workspaces.rs", "completed"),
        (
            "grep -n \"workspaces\\|thread::spawn\\|fn handle\" crates/sill/src/daemon/hub.rs",
            "completed",
        ),
        (
            "grep -rn \"dispatch(\" crates/sill/src/daemon/*.rs | head; grep -n \"fn serve\\|spawn\" crates/sill/src/daemon/hub.rs",
            "completed",
        ),
        (
            "sed -n 925,1000p crates/sill/src/daemon/hub.rs && sed -n 1225,1330p crates/sill/src/daemon/hub.rs",
            "completed",
        ),
        (
            "grep -n \"workspace\\|borrowed\" -i crates/sill-core/src/jail.rs | head -80",
            "completed",
        ),
        (
            "cd /home/demo/project && sed -n 540,640p crates/sill-core/src/jail.rs",
            "completed",
        ),
        ("grep -n \"pub fn\\|pub(crate) fn\" crates/sill-core/src/changes.rs", "failed"),
        ("cat crates/sill-core/src/workspace/carry.rs", "completed"),
        (
            "git for-each-ref | head -20; echo \"--- remotes:\"; git remote -v; echo \"--- tags: $(git tag | wc -l)\"",
            "completed",
        ),
        (
            "sed -i 's/add_note/add_error/; s/old;name/new/' crates/sill-gui/src/ui.rs",
            "completed",
        ),
        (
            "grep -rn \"interrupted\" --include=*.rs crates/sill-core/src/workspace; echo ----; grep -n \"conflict\" crates/sill-gui/src/workspaces.rs",
            "completed",
        ),
        (
            "for f in records flow carry; do wc -l crates/sill-core/src/workspace/$f.rs; done",
            "completed",
        ),
        ("sed -n 20,80p crates/sill-gui/src/workspaces_ui.rs", "in_progress"),
    ];
    let view = app.views.get_mut("demo").unwrap();
    let template = view.conversation.item("tool").unwrap().clone();
    for (n, (command, status)) in calls.iter().enumerate() {
        let item = Item {
            item_id: format!("tool-{n:02}"),
            tool: Some(Tool {
                title: (*command).into(),
                command: Some((*command).into()),
                status: (*status).into(),
                detail: format!("{command}\n(output)"),
                ..template.tool.clone().unwrap()
            }),
            ..template.clone()
        };
        view.conversation.insert_item(item);
    }
    let other = |id: &str, kind: &str, title: &str, status: &str| Item {
        item_id: id.into(),
        tool: Some(Tool {
            title: title.into(),
            kind: kind.into(),
            status: status.into(),
            command: None,
            ..template.tool.clone().unwrap()
        }),
        ..template.clone()
    };
    view.conversation.insert_item(other(
        "tool-97",
        "read",
        "Read file '/home/demo/project/docs/parallel-coding.md'",
        "completed",
    ));
    // a shell call whose input still streams: no command declared yet
    view.conversation
        .insert_item(other("tool-98", "execute", "Terminal", "pending"));
    render(&app, "12-tool-run");
    let _ = app.update(Message::ToggleGroup("tool-02".into()));
    render(&app, "13-tool-run-open");
    let _ = app.update(Message::ToggleTool("tool-05".into()));
    render(&app, "14-tool-run-call-open");
}

#[test]
#[ignore = "writes tab-bar PNGs for review"]
fn render_tabs() {
    let names = [
        "gray-blade", "humble-bell", "quiet-river", "a-much-longer-session-name", "ox",
        "silver-fox", "tidy-lamp", "brave-owl", "calm-sea", "dusty-road", "eager-elk", "fuzzy-moss",
    ];
    for count in [1, 2, 4, 6, 8, 9, 10, 12] {
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        let agent = app.tabs[0].agent.clone();
        app.tabs = names[..count]
            .iter()
            .map(|n| crate::Tab {
                id: n.to_string(),
                title: n.to_string(),
                agent: agent.clone(),
                running: true,
            })
            .collect();
        app.selected = Some(names[count.min(2) - 1].to_string());
        render_size(&app, &format!("tabs-{count}"), (720.0, 80.0));
    }
}

/// Three running tabs: the agent working (accent), writing sill's summary (success green, folded
/// to its glyph) and idle; the status-bar spinner follows the selected one, here the
/// summarizing tab.
#[test]
#[ignore = "writes the summarizing-tab PNG for review"]
fn render_tab_summarizing() {
    let (mut app, _) = App::new(true, None);
    app.demo = false;
    let agent = app.tabs[0].agent.clone();
    let template = app.views.remove("demo").unwrap();
    app.tabs.clear();
    for (id, summarizing, activity) in [("working", None, "working"), ("summarizing", Some("checkpoint"), "working"), ("idle", None, "idle")] {
        app.tabs.push(crate::Tab { id: id.into(), title: id.into(), agent: agent.clone(), running: true });
        let mut view = crate::SessionView::new(id);
        view.conversation = template.conversation.clone();
        let session = view.conversation.session.as_mut().unwrap();
        session.summarizing = summarizing.map(str::to_owned);
        session.activity = activity.into();
        if activity == "idle" {
            session.turn_id = None;
            session.active_since_ms = None;
        }
        view.status = template.status.clone();
        app.views.insert(id.into(), view);
    }
    app.selected = Some("summarizing".into());
    render_size(&app, "tabs-summarizing", (720.0, 860.0));
}

#[test]
#[ignore = "writes a hovered-tab PNG for review"]
fn render_tab_hover() {
    let (mut app, _) = App::new(true, None);
    app.demo = false;
    let agent = app.tabs[0].agent.clone();
    app.tabs = ["gray-blade", "a-much-longer-session-name", "ox", "silver-fox"]
        .iter()
        .map(|n| crate::Tab {
            id: n.to_string(),
            title: n.to_string(),
            agent: agent.clone(),
            running: true,
        })
        .collect();
    app.selected = Some("gray-blade".into());
    let _ = std::fs::remove_file(dir().join("tabs-hover-tiny-skia.png"));
    let mut ui = iced_test::Simulator::with_size(settings(), (720.0, 80.0), app.view());
    ui.point_at((260.0, 18.0));
    let snapshot = ui.snapshot(&crate::style::theme()).unwrap();
    assert!(snapshot.matches_image(dir().join("tabs-hover")).unwrap());
}

/// The Changes page by session: three sessions' workspaces of one repository with the files an
/// Apply would bring (one diff open), a session's writes outside its pins and a keep beside its
/// workspace, then the person's checkout and a discarded workspace.
#[test]
#[ignore = "writes the Changes page PNG for review"]
fn render_changes_workspaces() {
    use crate::Message;
    let (mut app, _) = App::new(true, None);
    let host = |p: &str| sill_proto::sessions::HostPath::from_path(std::path::Path::new(p));
    for (id, title, ended) in [("s1", "Changes view grouping", false), ("s2", "Restore previous active tab", true), ("s3", "Session tab layout fix", false)] {
        let mut s = crate::tests::session_record(id, "claude", ended, false);
        s.title = title.into();
        app.all_sessions.push(s);
    }
    let _ = app.update(Message::ToggleChanges);
    let sill = "/home/demo/GIT/sill";
    let live = |file: &str, status: &str| serde_json::json!({"type":"live", "repo": host(sill), "path": host(&format!("{sill}/{file}")), "status": status});
    let overlay = |session: &str, title: &str, file: &str, kind: &str| serde_json::json!({"type":"overlay", "session_id": session, "title": title,
            "ghost": false, "agent_state": false, "path": host(file), "kind": kind, "file_type":"file", "size":"120", "conflict": false, "written": "1793806135",
            "repo": null, "hash":"h", "protected": null});
    let entries: Vec<sill_proto::changes::Entry> = [
        live("crates/sill-gui/src/ui.rs", "M"),
        live("docs/notes.md", "?"),
        overlay("s1", "Changes view grouping", "/home/demo/.config/app/settings.toml", "modified"),
        overlay("s1", "Changes view grouping", "/home/demo/scratch/plan.md", "added"),
        serde_json::json!({"type":"kept", "session_id":"s3", "title":"Session tab layout fix", "ghost": false, "keep_id":"k1", "timestamp":"1793806135",
            "path": host("/home/demo/.bashrc"), "what":"wrote", "before":"a file", "hash": null}),
    ].into_iter().map(|e| serde_json::from_value(e).unwrap()).collect();
    let ws = |n: &str, session: &str, state: &str| serde_json::json!({"workspace_id": format!("ws-{n}"), "name": "sill", "session_id": session, "source": host(sill),
        "source_branch": "master", "base": "b0", "branch": format!("sill/sill-ws-{n}"), "path": host(&format!("/ws/sill-ws-{n}")),
        "state": state, "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"});
    let detail = |w: &serde_json::Value, commits: u32, files: &[(&str, &str, u32, u32)]| -> sill_proto::workspaces::Detail {
        let listed: Vec<serde_json::Value> = files.iter().map(|(path, status, added, removed)| serde_json::json!({"path": host(&format!("{sill}/{path}")), "status": status, "added": added.to_string(), "removed": removed.to_string()})).collect();
        serde_json::from_value(serde_json::json!({"workspace": w, "revision": {"tip": "t1", "worktree": "w", "destination_branch": "master", "destination_tip": "b0"},
            "summary": {"commits": commits, "files": files.len(), "added": files.iter().map(|f| f.2).sum::<u32>().to_string(), "removed": files.iter().map(|f| f.3).sum::<u32>().to_string(), "uncommitted": []},
            "needs_rebase": false, "files": listed})).unwrap()
    };
    let (w1, w2, w3) = (ws("6379eda6", "s1", "open"), ws("5a385ac0", "s2", "result"), ws("9a1e7978", "s3", "open"));
    let listed: sill_proto::workspaces::Listed = serde_json::from_value(serde_json::json!({"workspaces": [w1, w2, w3],
        "discarded": [{"discard_id": "d1", "workspace_id": "ws-old", "name": "sill", "source": host(sill), "until": "2026-11-17T10:00:00Z", "created_at": "t"}], "next": null})).unwrap();
    let details = vec![
        detail(&w1, 0, &[("crates/sill-client/src/changes.rs", "M", 96, 4), ("crates/sill-gui/src/changes_ui.rs", "M", 310, 180), ("docs/sill-gui.md", "M", 12, 3)]),
        detail(&w2, 1, &[("crates/sill-gui/src/main.rs", "M", 38, 6), ("crates/sill-gui/src/tests.rs", "M", 120, 0)]),
        detail(&w3, 0, &[("crates/sill-gui/src/old_tabs.rs", "D", 0, 64), ("crates/sill-gui/src/tabs.png", "A", 0, 0)]),
    ];
    let _ = app.update(Message::Workspaces(crate::workspaces::Event::PageLoaded(Ok((listed, details)))));
    let _ = app.update(Message::Changes(crate::changes_ui::Event::Loaded(Ok((entries, 1440)))));
    let _ = app.update(Message::Changes(crate::changes_ui::Event::WorkspaceDiff("ws-6379eda6".into(), host(&format!("{sill}/docs/sill-gui.md")))));
    let _ = app.update(Message::Changes(crate::changes_ui::Event::DiffLoaded(
        crate::changes_ui::workspace_key("ws-6379eda6", &host(&format!("{sill}/docs/sill-gui.md"))),
        Ok(demo::changes_diff()),
    )));
    render_size(&app, "15-changes-by-session", (720.0, 1000.0));
    // opened again: the listing from before, its actions waiting for the new one
    let _ = app.update(Message::ToggleChanges);
    let _ = app.update(Message::ToggleChanges);
    render_size(&app, "16-changes-reopened", (720.0, 1000.0));
}

/// The box band: waiting for a sign-in, SSH's question during one, and offline with a retry.
#[test]
#[ignore = "writes PNGs to target/sill-gui-snapshots"]
fn render_box_link() {
    use sill_proto::boxlink::{Problem, Prompt, PromptKind, Status};
    let (mut app, _) = App::new(true, None);
    for _ in 0..demo::CHUNKS.len() {
        let _ = app.demo_tick();
    }
    let status = |problem, detail: &str, retry_in| Status {
        host: "10.0.0.140".into(),
        problem,
        detail: detail.into(),
        retry_in,
        signing_in: false,
    };
    app.box_link.fake_status = Some(status(
        Problem::SignIn,
        "pat@10.0.0.140: Permission denied (publickey,password).",
        None,
    ));
    render(&app, "box-sign-in");
    let (reply, _answer) = std::sync::mpsc::channel();
    let _ = app.box_event(crate::box_ui::Event::Asked(crate::box_ui::Ask {
        prompt: Prompt {
            host: "10.0.0.140".into(),
            text: "Enter passphrase for key '/home/pat/.ssh/id_ed25519': ".into(),
            kind: PromptKind::Secret,
        },
        reply,
    }));
    let _ = app.box_event(crate::box_ui::Event::Input("open sesame".into()));
    render(&app, "box-question");
    let _ = app.box_event(crate::box_ui::Event::Cancel);
    app.box_link.fake_status = Some(status(
        Problem::Offline,
        "ssh: connect to host 10.0.0.140 port 22: No route to host",
        Some(std::time::Duration::from_secs(14)),
    ));
    render(&app, "box-offline");
}
