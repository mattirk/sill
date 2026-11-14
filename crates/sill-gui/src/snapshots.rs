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
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    // matches_image writes the file only when none exists; the renderer's name is appended
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{name}-"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut ui = iced_test::Simulator::with_size(settings(), (720.0, 860.0), app.view());
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
    let (mut empty, _) = App::new(true, None);
    empty.conversation = crate::model::Conversation::default();
    empty.choices.clear();
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
