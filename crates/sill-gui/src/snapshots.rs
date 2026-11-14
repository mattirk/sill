//! Renders views headlessly to PNG for visual review against `docs/reference-ui/`, at the shelf's
//! logical size (720×860; the simulator draws at scale 2, the captures' 1440×1720). Ignored by
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
    use crate::{Message, workspace::Action};
    use sill_proto::sessions::HostPath;
    let path = |s: &str| HostPath::from_path(std::path::Path::new(s));
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Workspace(Action::ToggleDrawer));
    let _ = app.update(Message::Workspace(Action::Toggle(path(
        "/home/demo/project/src",
    ))));
    render_size(&app, "6-drawer", (1000.0, 860.0));
    render(&app, "7-drawer-small");
    let _ = app.update(Message::Workspace(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: None,
    }));
    render_size(&app, "8-file", (1000.0, 860.0));
    let _ = app.update(Message::Workspace(Action::DiffMode(true)));
    let _ = app.update(Message::Workspace(Action::Open {
        path: path("/home/demo/project/README.md"),
        repo: Some(path("/home/demo/project")),
    }));
    render_size(&app, "9-diff", (1000.0, 860.0));
    render_size(&app, "10-narrow-drawer", (480.0, 700.0));
    // the agent waits on a question while the file is open: the strip keeps it in reach
    for _ in 0..=demo::CHUNKS.len() {
        let _ = app.update(Message::DemoTick);
    }
    render_size(&app, "11-strip", (1000.0, 860.0));
}
