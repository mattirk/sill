//! Document links, Back/Forward and heading jumps.
use super::*;

#[test]
fn document_links_resolve_against_the_document_not_the_process_directory() {
    let source = path("/project/docs/start.md");
    for (target, expected, anchor) in [
        ("other.md", "/project/docs/other.md", ""),
        (
            "../README.md#some-heading",
            "/project/README.md",
            "some-heading",
        ),
        ("./a%20b.md#caf%C3%A9", "/project/docs/a b.md", "café"),
        ("#here", "/project/docs/start.md", "here"),
        ("/elsewhere/file.md", "/elsewhere/file.md", ""),
    ] {
        let LinkTarget::File(actual, fragment) = resolve_link(&source, target).unwrap() else {
            panic!("expected file")
        };
        assert_eq!(actual.to_path().unwrap(), std::path::Path::new(expected));
        assert_eq!(fragment, anchor);
    }
    assert!(matches!(
        resolve_link(&source, "https://example.org/a#b").unwrap(),
        LinkTarget::Web(_)
    ));
    for target in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "//example.org/a",
        "%00.md",
    ] {
        assert!(resolve_link(&source, target).is_err(), "{target}");
    }
}

#[test]
fn heading_links_match_formatted_unicode_and_duplicate_headings() {
    let content =
        markdown::Content::parse("# Hello, **world**!\n\n# Hello, world!\n\n# Café `code`\n");
    assert_eq!(heading_index(content.items(), "hello-world"), Some(0));
    assert_eq!(heading_index(content.items(), "hello-world-1"), Some(1));
    assert_eq!(heading_index(content.items(), "café-code"), Some(2));
    assert_eq!(heading_index(content.items(), "missing"), None);
}

#[test]
fn document_links_keep_history_guard_edits_and_ignore_stale_sources() {
    let mut app = app();
    let first = path("/home/demo/project/README.md");
    act(
        &mut app,
        Action::Open {
            path: first.clone(),
            repo: None,
        },
    );
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: first.clone(),
            target: "docs/next.md".into(),
        },
    );
    let second = files(&app, "demo").reader.as_ref().unwrap().path.clone();
    assert_eq!(second.display, "/home/demo/project/docs/next.md");
    act(&mut app, Action::Back(false));
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        first.display
    );
    act(&mut app, Action::Back(true));
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: first.clone(),
            target: "stale.md".into(),
        },
    );
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    // Mark a preview buffer dirty, as when editing source then switching back to preview.
    app.views
        .get_mut("demo")
        .unwrap()
        .reader
        .as_mut()
        .unwrap()
        .dirty = true;
    act(
        &mut app,
        Action::Link {
            session: "demo".into(),
            source: second.clone(),
            target: "third.md".into(),
        },
    );
    assert!(app.drawer.pending_discard.is_some());
    assert_eq!(
        files(&app, "demo").reader.as_ref().unwrap().path.display,
        second.display
    );
    act(&mut app, Action::CancelDiscard);
    act(&mut app, Action::Back(false));
    assert!(app.drawer.pending_discard.is_some());
    assert_eq!(app.drawer.navigation["demo"].back.len(), 1);
}

#[test]
fn heading_jump_finds_the_heading_in_the_laid_out_reader() {
    use iced::advanced::{
        renderer::Headless,
        widget::{Operation, operation},
    };
    let mut app = app();
    act(
        &mut app,
        Action::Open {
            path: path("/home/demo/project/README.md"),
            repo: None,
        },
    );
    let view = app.views.get_mut("demo").unwrap();
    let file = view.files.reader.as_mut().unwrap();
    file.text = format!(
        "# Start\n\n{}\n\n# Destination\n",
        "paragraph\n\n".repeat(50)
    );
    file.revision += 1;
    view.sync_reader();
    let items = view
        .reader
        .as_ref()
        .unwrap()
        .markdown
        .as_ref()
        .unwrap()
        .items();
    let index = heading_index(items, "destination").unwrap();
    let settings = crate::snapshots::settings();
    let mut renderer = iced::futures::executor::block_on(<iced::Renderer as Headless>::new(
        settings.default_font,
        settings.default_text_size,
        None,
    ))
    .unwrap();
    let mut ui = iced_runtime::UserInterface::build(
        app.view(),
        iced::Size::new(1000.0, 860.0),
        iced_runtime::user_interface::Cache::default(),
        &mut renderer,
    );
    let mut find = FindHeading {
        target: iced::advanced::widget::Id::from(format!("markdown-heading-{index}")),
        session: "demo".into(),
        key: "test".into(),
        origin: 0.0,
        y: None,
    };
    ui.operate(&renderer, &mut operation::black_box(&mut find));
    assert!(
        matches!(find.finish(), operation::Outcome::Some(Message::Files(Action::AnchorOffset { y, .. })) if y > 860.0)
    );
}

#[test]
fn conversation_markdown_links_open_in_the_session_reader_and_guard_edits() {
    let mut app = app();
    let _ = app.update(Message::Link(
        "/Users/demo/project/packaging/RELEASING.md#mac-image-and-packaging".into(),
    ));
    let file = files(&app, "demo").reader.as_ref().expect("conversation link opens reader");
    assert_eq!(file.path.display, "/Users/demo/project/packaging/RELEASING.md");
    // Demo files load immediately; its placeholder document lacks the requested heading.
    assert_eq!(app.views["demo"].reader.as_ref().unwrap().notice.as_deref(),
        Some("Heading not found: #mac-image-and-packaging"));
    assert!(files(&app, "b").reader.is_none(), "other sessions stay unchanged");
    let _ = app.update(Message::Link("/project/a%20b.md".into()));
    assert_eq!(files(&app, "demo").reader.as_ref().unwrap().path.display, "/project/a b.md");
    app.views.get_mut("demo").unwrap().reader.as_mut().unwrap().dirty = true;
    let _ = app.update(Message::Link("/project/next.md".into()));
    assert!(app.drawer.pending_discard.is_some());
    assert_eq!(files(&app, "demo").reader.as_ref().unwrap().path.display, "/project/a b.md");
}
