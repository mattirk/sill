use crate::tests::{daemon, live_app, settle_task};
use crate::{
    App, Message, SessionView,
    annotation::{Event, Stroke},
    media,
};
use iced::Point;
use sill_proto::method;
use sill_proto::{
    admin,
    sfp::{Attachment, Submit},
};

fn image() -> (Attachment, media::Picture) {
    let rgba = ::image::RgbaImage::from_pixel(32, 24, ::image::Rgba([255, 255, 255, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    ::image::DynamicImage::ImageRgba8(rgba).write_to(&mut bytes, ::image::ImageFormat::Png).unwrap();
    let bytes = bytes.into_inner();
    let picture = media::decode(&bytes).unwrap();
    (Attachment::Inline { name: "screenshot.png".into(), mime: "image/png".into(), data_base64: admin::encode_bytes(&bytes) }, picture)
}

fn attach(app: &mut App, session: &str, count: usize) -> Attachment {
    let (a, pic) = image();
    let _ = app.update(Message::Media(media::Event::Attached(session.into(), Ok((0..count).map(|_| (a.clone(), Some(pic.clone()))).collect()))));
    a
}

fn open(app: &mut App, index: usize) {
    let id = app.current().unwrap().attachments[index].id.clone();
    let _ = app.update(Message::Annotation(Event::Open(id)));
    assert!(app.current().unwrap().annotation.is_some());
}

fn stroke(app: &mut App, y: f32) {
    let _ = app.update(Message::Annotation(Event::Stroke(Stroke { points: vec![Point::new(4.0, y), Point::new(24.0, y)], width: 3.0 })));
}

fn save(app: &mut App) {
    let task = app.update(Message::Annotation(Event::Save));
    settle_task(app, task);
    assert!(app.current().unwrap().annotation.is_none());
}

#[test]
fn annotation_duplicate_images_edit_independently_and_reopen_with_original_strokes() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 2);
    let ids: Vec<_> = app.views["demo"].attachments.iter().map(|a| a.id.clone()).collect();
    assert_ne!(ids[0], ids[1]);
    open(&mut app, 0);
    stroke(&mut app, 8.0);
    save(&mut app);
    let first_saved = app.views["demo"].attachments[0].attachment.clone();
    assert_ne!(first_saved, original);
    assert_eq!(app.views["demo"].attachments[1].attachment, original);
    assert!(app.views["demo"].attachments[1].annotation.is_none());

    open(&mut app, 0);
    let editor = app.views["demo"].annotation.as_ref().unwrap();
    assert_eq!(editor.original, original);
    assert_eq!(editor.strokes.len(), 1);
    stroke(&mut app, 16.0);
    let _ = app.update(Message::Annotation(Event::Cancel));
    assert_eq!(app.views["demo"].attachments[0].attachment, first_saved);
    open(&mut app, 0);
    assert_eq!(app.views["demo"].annotation.as_ref().unwrap().strokes.len(), 1);
    let _ = app.update(Message::Annotation(Event::Undo));
    save(&mut app);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
    assert!(app.views["demo"].attachments[0].annotation.is_none());

    open(&mut app, 1);
    stroke(&mut app, 8.0);
    stroke(&mut app, 16.0);
    save(&mut app);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
    open(&mut app, 1);
    assert_eq!(app.views["demo"].annotation.as_ref().unwrap().strokes.len(), 2);
    let _ = app.update(Message::Annotation(Event::Clear));
    save(&mut app);
    assert_eq!(app.views["demo"].attachments[1].attachment, original);
    assert_eq!(app.views["demo"].attachments[0].id, ids[0]);
    assert_eq!(app.views["demo"].attachments[1].id, ids[1]);
}

#[test]
fn annotation_blocks_send_and_prompt_changes_until_saved_or_cancelled() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 1);
    app.views.get_mut("demo").unwrap().draft = crate::editor::Editor::with_text("fix this button");
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    let task = app.update(crate::tests::in_session(&app, crate::session::Event::Send));
    settle_task(&mut app, task);
    let _ = app.update(crate::tests::in_session(
        &app,
        crate::session::Event::Prompt(iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
            "unexpected".into(),
        )))),
    ));
    let _ = app.update(Message::Media(media::Event::Remove(0)));
    let view = &app.views["demo"];
    assert_eq!(view.draft.text().trim_end(), "fix this button");
    assert_eq!(view.attachments[0].attachment, original);
    assert!(view.outbox.pending().is_empty());
    assert!(!view.conversation_shown());

    let task = app.update(Message::Annotation(Event::Save));
    assert!(app.views["demo"].annotation.as_ref().unwrap().saving);
    let blocked = app.update(crate::tests::in_session(&app, crate::session::Event::Send));
    settle_task(&mut app, blocked);
    assert_eq!(app.views["demo"].draft.text().trim_end(), "fix this button");
    assert_eq!(app.views["demo"].attachments.len(), 1);
    settle_task(&mut app, task);
    assert!(app.views["demo"].annotation.is_none());
    assert!(app.views["demo"].conversation_shown());
}

#[test]
fn annotation_async_save_stays_with_originating_session() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 1);
    app.views.insert("other".into(), SessionView::new("other"));
    attach(&mut app, "other", 1);
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    let task = app.update(Message::Annotation(Event::Save));
    app.selected = Some("other".into());
    open(&mut app, 0);
    let other_revision = app.views["other"].annotation.as_ref().unwrap().revision.clone();
    settle_task(&mut app, task);
    assert_ne!(app.views["demo"].attachments[0].attachment, original);
    assert!(app.views["demo"].annotation.is_none());
    assert_eq!(app.views["other"].attachments[0].attachment, original);
    assert_eq!(app.views["other"].annotation.as_ref().unwrap().revision, other_revision);
    assert_eq!(app.selected.as_deref(), Some("other"));
}

#[test]
fn annotation_cancel_or_recreated_view_rejects_late_save() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 1);
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    let task = app.update(Message::Annotation(Event::Save));
    let old_revision = app.views["demo"].annotation.as_ref().unwrap().revision.clone();
    let _ = app.update(Message::Annotation(Event::Cancel));
    open(&mut app, 0);
    assert_ne!(app.views["demo"].annotation.as_ref().unwrap().revision, old_revision);
    settle_task(&mut app, task);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
    assert!(app.views["demo"].annotation.as_ref().unwrap().strokes.is_empty());

    stroke(&mut app, 8.0);
    let task = app.update(Message::Annotation(Event::Save));
    let old_token = app.views["demo"].media_token.clone();
    app.views.insert("demo".into(), SessionView::new("demo"));
    attach(&mut app, "demo", 1);
    open(&mut app, 0);
    assert_ne!(app.views["demo"].media_token, old_token);
    settle_task(&mut app, task);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
    assert!(app.views["demo"].annotation.as_ref().unwrap().strokes.is_empty());
    assert!(!app.views["demo"].annotation.as_ref().unwrap().saving);
}

#[test]
fn annotation_save_failure_keeps_original_attachment_and_editable_work() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 1);
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    let view = app.views.get_mut("demo").unwrap();
    let editor = view.annotation.as_mut().unwrap();
    editor.saving = true;
    let event = Event::Saved {
        session: "demo".into(),
        token: view.media_token.clone(),
        attachment_id: editor.attachment_id.clone(),
        revision: editor.revision.clone(),
        result: Err("PNG encoding failed".into()),
    };
    let _ = app.update(Message::Annotation(event));
    let editor = app.views["demo"].annotation.as_ref().unwrap();
    assert!(!editor.saving);
    assert_eq!(editor.strokes.len(), 1);
    assert_eq!(editor.error.as_deref(), Some("PNG encoding failed"));
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
    save(&mut app);
    assert_ne!(app.views["demo"].attachments[0].attachment, original);
}

#[test]
fn annotation_failed_send_restores_editable_markers_and_stable_attachment_identity() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    let original = attach(&mut app, "demo", 1);
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    save(&mut app);
    let attachment = app.views["demo"].attachments[0].clone();
    let task = app.update(crate::tests::in_session(&app, crate::session::Event::Send));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    let submit: Submit = serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(submit.attachments, vec![attachment.attachment.clone()]);
    assert_ne!(submit.attachments[0], original);
    daemon.refuse(&request, "invalid_state", "session stopped");
    settle_task(&mut app, task);
    let restored = &app.views["demo"].attachments[0];
    assert_eq!(restored.id, attachment.id);
    assert_eq!(restored.attachment, attachment.attachment);
    assert_eq!(restored.annotation.as_ref().unwrap().strokes.len(), 1);
    assert!(app.views["demo"].pending_attachments.is_empty());
    open(&mut app, 0);
    let _ = app.update(Message::Annotation(Event::Undo));
    save(&mut app);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
}

#[test]
fn annotation_uncertain_retry_preserves_exact_flattened_bytes_and_submission_identity() {
    let (client, mut first) = daemon();
    let (next_client, mut second) = daemon();
    let mut app = live_app(vec![client, next_client]);
    attach(&mut app, "demo", 1);
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    save(&mut app);
    let expected = app.views["demo"].attachments[0].attachment.clone();
    let task = app.update(crate::tests::in_session(&app, crate::session::Event::Send));
    let request = first.expect(method::SESSIONS_SUBMIT);
    let submit: Submit = serde_json::from_value(request["params"].clone()).unwrap();
    assert_eq!(submit.attachments, vec![expected]);
    drop(first);
    settle_task(&mut app, task);
    assert!(matches!(app.views["demo"].outbox.get(&submit.submission_id).unwrap().state, sill_client::outbox::State::Uncertain(_)));
    assert_eq!(app.views["demo"].pending_attachments[&submit.submission_id][0].annotation.as_ref().unwrap().strokes.len(), 1);
    let retry = app.update(crate::tests::in_session(&app, crate::session::Event::RetrySend(submit.submission_id.clone())));
    let retried = second.expect(method::SESSIONS_SUBMIT);
    assert_eq!(retried["params"], request["params"]);
    second.reply(&retried, serde_json::json!({}));
    settle_task(&mut app, retry);
    assert!(app.views["demo"].pending_attachments.is_empty());
    assert!(app.views["demo"].outbox.pending().is_empty());
    assert_eq!(app.views["demo"].decoded_media_bytes(), 0);
    assert!(!app.views["demo"].media.keys().any(|key| key.starts_with("draft:")));
}

#[test]
fn annotation_accounts_for_original_pixels_and_releases_replaced_or_discarded_previews() {
    let (client, mut daemon) = daemon();
    let mut app = live_app(vec![client]);
    attach(&mut app, "demo", 1);
    let bytes = 32 * 24 * 4;
    assert_eq!(app.views["demo"].decoded_media_bytes(), bytes);
    open(&mut app, 0);
    assert_eq!(app.views["demo"].decoded_media_bytes(), bytes, "editor shares original pixels");
    stroke(&mut app, 8.0);
    save(&mut app);
    assert_eq!(app.views["demo"].decoded_media_bytes(), 2 * bytes, "original and flattened preview both count");
    let first_key = media::key(&app.views["demo"].attachments[0]);
    open(&mut app, 0);
    stroke(&mut app, 16.0);
    save(&mut app);
    assert_eq!(app.views["demo"].decoded_media_bytes(), 2 * bytes, "replaced preview is released");
    assert!(!app.views["demo"].media.contains_key(&first_key));
    let task = app.update(crate::tests::in_session(&app, crate::session::Event::Send));
    let request = daemon.expect(method::SESSIONS_SUBMIT);
    let id = request["params"]["submission_id"].as_str().unwrap().to_string();
    assert_eq!(app.views["demo"].decoded_media_bytes(), 2 * bytes, "pending send retains editable original");
    drop(daemon);
    settle_task(&mut app, task);
    let _ = app.update(crate::tests::in_session(&app, crate::session::Event::DiscardSend(id)));
    assert!(app.views["demo"].pending_attachments.is_empty());
    assert_eq!(app.views["demo"].decoded_media_bytes(), 0);
    assert!(!app.views["demo"].media.keys().any(|key| key.starts_with("draft:")));
}

#[test]
fn annotation_active_gesture_blocks_save_and_undo_until_stroke_completion() {
    let (mut app, _) = App::new(true, None);
    attach(&mut app, "demo", 1);
    open(&mut app, 0);
    stroke(&mut app, 8.0);
    let _ = app.update(Message::Annotation(Event::Drawing(true)));
    let task = app.update(Message::Annotation(Event::Save));
    settle_task(&mut app, task);
    let _ = app.update(Message::Annotation(Event::Undo));
    let _ = app.update(Message::Annotation(Event::Clear));
    let editor = app.views["demo"].annotation.as_ref().unwrap();
    assert!(editor.drawing);
    assert!(!editor.saving);
    assert_eq!(editor.strokes.len(), 1);
    stroke(&mut app, 16.0);
    assert!(!app.views["demo"].annotation.as_ref().unwrap().drawing);
    assert_eq!(app.views["demo"].annotation.as_ref().unwrap().strokes.len(), 2);
    let _ = app.update(Message::Annotation(Event::Undo));
    assert_eq!(app.views["demo"].annotation.as_ref().unwrap().strokes.len(), 1);
    save(&mut app);
}

#[test]
fn annotation_stale_scoped_stroke_cannot_edit_a_different_editor() {
    let (mut app, _) = App::new(true, None);
    attach(&mut app, "demo", 2);
    open(&mut app, 0);
    let old_revision = app.views["demo"].annotation.as_ref().unwrap().revision.clone();
    let _ = app.update(Message::Annotation(Event::Cancel));
    open(&mut app, 1);
    let _ = app.update(Message::Annotation(Event::Scoped(
        old_revision.clone(),
        Box::new(Event::Stroke(Stroke { points: vec![Point::new(4.0, 8.0), Point::new(24.0, 8.0)], width: 3.0 })),
    )));
    let _ = app.update(Message::Annotation(Event::Scoped(old_revision, Box::new(Event::Drawing(true)))));
    let editor = app.views["demo"].annotation.as_ref().unwrap();
    assert!(editor.strokes.is_empty());
    assert!(!editor.drawing);
}

#[test]
fn annotation_save_checks_combined_inline_limit_without_losing_work() {
    let (mut app, _) = App::new(true, None);
    let original = attach(&mut app, "demo", 1);
    // A separate inline attachment consumes the whole submission budget. Construct it
    // directly: the normal chooser rejects this combination before reaching the editor.
    app.views.get_mut("demo").unwrap().attachments.push(
        Attachment::Inline {
            name: "full.bin".into(),
            mime: "application/octet-stream".into(),
            data_base64: admin::encode_bytes(&vec![0; sill_proto::initialize::MAX_INLINE_BYTES]),
        }
        .into(),
    );
    open(&mut app, 0);
    stroke(&mut app, 12.0);
    let task = app.update(Message::Annotation(Event::Save));
    settle_task(&mut app, task);
    let editor = app.views["demo"].annotation.as_ref().unwrap();
    assert!(editor.error.as_ref().unwrap().contains("10 MiB"));
    assert!(!editor.saving);
    assert_eq!(editor.strokes.len(), 1);
    assert_eq!(app.views["demo"].attachments[0].attachment, original);
}

#[test]
#[ignore = "writes annotation editor and saved-draft PNGs for visual review"]
fn annotation_editor_visual_snapshots() {
    // A synthetic application screenshot: toolbar, navigation, content rows and an action.
    // This stays self-contained and makes marker alignment obvious in the exported previews.
    let mut screenshot = ::image::RgbaImage::from_pixel(960, 600, ::image::Rgba([245, 247, 250, 255]));
    let mut rect = |x, y, width, height, color| {
        for py in y..y + height {
            for px in x..x + width {
                screenshot.put_pixel(px, py, ::image::Rgba(color));
            }
        }
    };
    rect(0, 0, 960, 58, [35, 43, 60, 255]);
    rect(0, 58, 170, 542, [222, 228, 237, 255]);
    rect(25, 21, 100, 16, [234, 239, 248, 255]);
    for y in [90, 135, 180, 225] {
        rect(22, y, 118, 13, [130, 145, 164, 255]);
    }
    rect(205, 94, 280, 24, [51, 63, 82, 255]);
    rect(205, 141, 510, 12, [159, 170, 187, 255]);
    for y in [198, 270, 342, 414] {
        rect(205, y, 710, 54, [255, 255, 255, 255]);
        rect(225, y + 19, 240, 12, [123, 137, 157, 255]);
    }
    rect(737, 499, 178, 55, [49, 109, 214, 255]);
    rect(777, 520, 98, 13, [244, 248, 255, 255]);
    let mut png = std::io::Cursor::new(Vec::new());
    ::image::DynamicImage::ImageRgba8(screenshot).write_to(&mut png, ::image::ImageFormat::Png).unwrap();
    let bytes = png.into_inner();
    let picture = media::decode(&bytes).unwrap();
    let (mut app, _) = App::new(true, None);
    let _ = app.update(Message::Media(media::Event::Attached(
        "demo".into(),
        Ok(vec![(Attachment::Inline { name: "application.png".into(), mime: "image/png".into(), data_base64: admin::encode_bytes(&bytes) }, Some(picture))]),
    )));
    app.views.get_mut("demo").unwrap().draft = crate::editor::Editor::with_text("Make the marked button easier to notice.");
    crate::snapshots::render(&app, "annotation-before");
    open(&mut app, 0);
    let circle = (0..=80)
        .map(|i| {
            let angle = i as f32 / 80.0 * std::f32::consts::TAU;
            Point::new(823.0 + 119.0 * angle.cos(), 526.0 + 48.0 * angle.sin())
        })
        .collect();
    let _ = app.update(Message::Annotation(Event::Stroke(Stroke { points: circle, width: 5.0 })));
    let _ = app.update(Message::Annotation(Event::Stroke(Stroke {
        points: vec![Point::new(565.0, 460.0), Point::new(680.0, 504.0), Point::new(659.0, 481.0), Point::new(680.0, 504.0), Point::new(647.0, 508.0)],
        width: 5.0,
    })));
    crate::snapshots::render(&app, "annotation-editor");
    crate::snapshots::render_size(&app, "annotation-editor-narrow", (480.0, 700.0));
    save(&mut app);
    crate::snapshots::render(&app, "annotation-saved");
    open(&mut app, 0);
    crate::snapshots::render(&app, "annotation-reopened");
}
