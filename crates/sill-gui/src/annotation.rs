//! Draft-only freehand annotations. Coordinates and widths are stored in source pixels.
use crate::{Message, keys, media::Picture, style};
use iced::widget::{button, canvas, column, container, row, space, stack, text};
use iced::{Color, Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};
use sill_proto::{admin, sfp::Attachment};
use std::sync::Arc;

const RED: Color = Color::from_rgb(1.0, 0.0, 0.0);

#[derive(Debug, Clone)]
pub struct Stroke {
    pub points: Vec<Point>,
    pub width: f32,
}

#[derive(Debug, Clone)]
pub struct Saved {
    pub original: Attachment,
    pub picture: Picture,
    pub strokes: Vec<Stroke>,
}

#[derive(Debug, Clone)]
pub struct Editor {
    pub attachment_id: String,
    pub revision: String,
    pub original: Attachment,
    pub picture: Picture,
    pub strokes: Vec<Stroke>,
    pub saving: bool,
    pub drawing: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Scoped(String, Box<Event>),
    Open(String),
    Stroke(Stroke),
    Drawing(bool),
    Undo,
    Clear,
    Cancel,
    Save,
    Saved { session: String, token: String, attachment_id: String, revision: String, result: Result<(Attachment, Picture), String> },
}

pub fn view(editor: &Editor) -> Element<'_, Message> {
    let control = |label: &'static str, event, enabled: bool| {
        keys::press_maybe(
            button(text(label).size(style::SMALL)).style(style::quiet),
            enabled.then_some(Message::Annotation(Event::Scoped(editor.revision.clone(), Box::new(event)))),
        )
    };
    let toolbar = row![
        control("Undo", Event::Undo, !editor.saving && !editor.drawing && !editor.strokes.is_empty()),
        control("Clear", Event::Clear, !editor.saving && !editor.drawing && !editor.strokes.is_empty()),
        space().width(Fill),
        control("Cancel", Event::Cancel, !editor.saving),
        control(if editor.saving { "Saving…" } else { "Save" }, Event::Save, !editor.saving && !editor.drawing),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    let mut content = column![text("Mark image").size(style::SMALL), toolbar, surface(editor)].spacing(8);
    if let Some(error) = &editor.error {
        content = content.push(text(error).size(style::SMALL).color(style::P.error));
    }
    container(content).padding(12).width(Fill).height(Fill).into()
}

/// Separate stack layers are essential: Iced batches images over canvas meshes.
fn surface(editor: &Editor) -> Element<'_, Message> {
    let image = iced::widget::image(editor.picture.handle.clone()).width(Fill).height(Fill).content_fit(iced::ContentFit::Contain);
    let background = container(image).width(Fill).height(Fill).style(|_| container::Style { background: Some(style::P.code_bg.into()), ..Default::default() });
    stack![background, canvas(editor).width(Fill).height(Fill)].width(Fill).height(Fill).clip(true).into()
}

/// The fitted image rectangle in canvas-local coordinates, including letterboxing.
pub fn fit_rect(image: Size, bounds: Size) -> Rectangle {
    if image.width <= 0.0 || image.height <= 0.0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Rectangle::default();
    }
    let scale = (bounds.width / image.width).min(bounds.height / image.height);
    let width = image.width * scale;
    let height = image.height * scale;
    Rectangle { x: (bounds.width - width) / 2.0, y: (bounds.height - height) / 2.0, width, height }
}

fn source_point(at: Point, fitted: Rectangle, image: Size) -> Point {
    Point::new((at.x - fitted.x) * image.width / fitted.width, (at.y - fitted.y) * image.height / fitted.height)
}

/// End a segment at its first image boundary, rather than drawing along the border.
fn clipped_end(from: Point, to: Point, image: Size) -> Point {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let mut t = 1.0_f32;
    if to.x < 0.0 && dx < 0.0 {
        t = t.min(-from.x / dx);
    }
    if to.x > image.width && dx > 0.0 {
        t = t.min((image.width - from.x) / dx);
    }
    if to.y < 0.0 && dy < 0.0 {
        t = t.min(-from.y / dy);
    }
    if to.y > image.height && dy > 0.0 {
        t = t.min((image.height - from.y) / dy);
    }
    Point::new((from.x + t * dx).clamp(0.0, image.width), (from.y + t * dy).clamp(0.0, image.height))
}

#[derive(Default)]
pub struct Gesture {
    owner: String,
    active: Option<Stroke>,
    bounds: Size,
    cursor_inside: bool,
}

impl Gesture {
    fn finish(&mut self) -> Option<canvas::Action<Message>> {
        self.active
            .take()
            .map(|stroke| canvas::Action::publish(Message::Annotation(Event::Scoped(self.owner.clone(), Box::new(Event::Stroke(stroke))))).and_capture())
    }
}

impl canvas::Program<Message> for Editor {
    type State = Gesture;

    fn update(&self, state: &mut Gesture, event: &canvas::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        if state.owner != self.revision {
            state.owner.clone_from(&self.revision);
            state.active = None;
            state.cursor_inside = false;
        }
        if self.saving {
            state.active = None;
            state.cursor_inside = false;
            return None;
        }
        // Switching sessions can replace the widget tree while a button is held.
        // Its unfinished local gesture is gone, so release the editor's drawing guard.
        if self.drawing && state.active.is_none() {
            return Some(canvas::Action::publish(Message::Annotation(Event::Scoped(self.revision.clone(), Box::new(Event::Drawing(false))))));
        }
        let image = Size::new(self.picture.width as f32, self.picture.height as f32);
        let fitted = fit_rect(image, bounds.size());
        if fitted.width <= 0.0 || fitted.height <= 0.0 {
            return None;
        }
        if state.active.is_some() && state.bounds != bounds.size() {
            return state.finish();
        }
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                if !fitted.contains(at) {
                    return None;
                }
                state.cursor_inside = true;
                state.bounds = bounds.size();
                state.active = Some(Stroke { points: vec![source_point(at, fitted, image)], width: 3.0 * image.width / fitted.width });
                Some(canvas::Action::publish(Message::Annotation(Event::Scoped(self.revision.clone(), Box::new(Event::Drawing(true))))).and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.active.is_some() => {
                let Some(at) = cursor.position_from(bounds.position()) else {
                    state.cursor_inside = false;
                    return state.finish();
                };
                let stroke = state.active.as_mut().unwrap();
                let point = source_point(at, fitted, image);
                let last = *stroke.points.last().unwrap();
                let end = clipped_end(last, point, image);
                if end.distance(last) * fitted.width / image.width >= 0.25 {
                    stroke.points.push(end);
                }
                if !fitted.contains(at) || stroke.points.len() >= 100_000 {
                    state.cursor_inside = fitted.contains(at);
                    state.finish()
                } else {
                    state.cursor_inside = true;
                    Some(canvas::Action::request_redraw().and_capture())
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let inside = cursor.position_in(bounds).is_some_and(|at| fitted.contains(at));
                let was_inside = std::mem::replace(&mut state.cursor_inside, inside);
                if inside || was_inside { Some(canvas::Action::request_redraw()) } else { None }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | iced::Event::Mouse(mouse::Event::CursorLeft)
            | iced::Event::Window(iced::window::Event::Unfocused) => {
                if state.active.is_some() {
                    if matches!(event, iced::Event::Mouse(mouse::Event::CursorLeft) | iced::Event::Window(iced::window::Event::Unfocused)) {
                        state.cursor_inside = false;
                    }
                    state.finish()
                } else if std::mem::take(&mut state.cursor_inside) {
                    Some(canvas::Action::request_redraw())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn draw(&self, state: &Gesture, renderer: &Renderer, _theme: &Theme, bounds: Rectangle, cursor: mouse::Cursor) -> Vec<canvas::Geometry> {
        let image = Size::new(self.picture.width as f32, self.picture.height as f32);
        let fitted = fit_rect(image, bounds.size());
        if fitted.width > 0.0 && fitted.height > 0.0 {
            // A separate frame keeps the image clip in both Iced renderers. A draft
            // frame does not translate its origin to the clip rectangle.
            let mut marks = canvas::Frame::with_bounds(renderer, fitted);
            marks.translate(iced::Vector::new(fitted.x, fitted.y));
            let scale = fitted.width / image.width;
            let active = state.active.as_ref().filter(|_| state.owner == self.revision && !self.saving);
            for stroke in self.strokes.iter().chain(active) {
                draw_stroke(&mut marks, stroke, scale);
            }
            let mut layers = vec![marks.into_geometry()];
            if !self.saving
                && let Some(at) = cursor.position_in(bounds).filter(|at| fitted.contains(*at))
            {
                let mut pointer = canvas::Frame::new(renderer, bounds.size());
                draw_pen_cursor(&mut pointer, at);
                layers.push(pointer.into_geometry());
            }
            return layers;
        }
        vec![]
    }

    fn mouse_interaction(&self, _state: &Gesture, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        let fitted = fit_rect(Size::new(self.picture.width as f32, self.picture.height as f32), bounds.size());
        if !self.saving && cursor.position_in(bounds).is_some_and(|at| fitted.contains(at)) {
            mouse::Interaction::Hidden
        } else {
            mouse::Interaction::default()
        }
    }
}

/// Draw a pen cursor anchored with its tip at `tip`, visible on both light and dark images.
fn draw_pen_cursor(frame: &mut canvas::Frame, tip: Point) {
    let scale = 20.0 / 24.0;
    let pt = |x: f32, y: f32| Point::new(tip.x + (x - 3.0) * scale, tip.y + (y - 21.0) * scale);
    let outline = canvas::Path::new(|p| {
        p.move_to(pt(3.0, 21.0));
        p.line_to(pt(4.0, 16.5));
        p.line_to(pt(16.5, 4.0));
        p.line_to(pt(18.5, 3.0));
        p.line_to(pt(21.0, 5.5));
        p.line_to(pt(20.0, 7.5));
        p.line_to(pt(7.5, 20.0));
        p.close();
    });
    let nib = canvas::Path::new(|p| {
        p.move_to(pt(3.0, 21.0));
        p.line_to(pt(4.0, 16.5));
        p.line_to(pt(7.5, 20.0));
        p.close();
    });
    let details = canvas::Path::new(|p| {
        p.move_to(pt(4.0, 16.5));
        p.line_to(pt(7.5, 20.0));
        p.move_to(pt(15.0, 5.5));
        p.line_to(pt(18.5, 9.0));
    });
    let dark = Color::from_rgb(0.10, 0.10, 0.10);
    frame.stroke(
        &outline,
        canvas::Stroke::default()
            .with_color(Color::from_rgba(0.05, 0.05, 0.05, 0.9))
            .with_width(2.8)
            .with_line_cap(canvas::LineCap::Round)
            .with_line_join(canvas::LineJoin::Round),
    );
    frame.fill(&outline, Color::WHITE);
    frame.fill(&nib, RED);
    frame.stroke(
        &outline,
        canvas::Stroke::default().with_color(dark).with_width(1.1).with_line_cap(canvas::LineCap::Round).with_line_join(canvas::LineJoin::Round),
    );
    frame.stroke(&details, canvas::Stroke::default().with_color(dark).with_width(1.1).with_line_cap(canvas::LineCap::Round));
}

fn draw_stroke(frame: &mut canvas::Frame, stroke: &Stroke, scale: f32) {
    let Some(first) = stroke.points.first() else {
        return;
    };
    let at = |point: Point| Point::new(point.x * scale, point.y * scale);
    if stroke.points.len() == 1 {
        frame.fill(&canvas::Path::circle(at(*first), stroke.width * scale / 2.0), RED);
    } else {
        let path = canvas::Path::new(|path| {
            path.move_to(at(*first));
            for point in &stroke.points[1..] {
                path.line_to(at(*point));
            }
        });
        frame.stroke(
            &path,
            canvas::Stroke::default()
                .with_color(RED)
                .with_width(stroke.width * scale)
                .with_line_cap(canvas::LineCap::Round)
                .with_line_join(canvas::LineJoin::Round),
        );
    }
}

/// Flatten onto a fresh buffer. Neither the source attachment nor its pixels are changed.
pub fn render(original: &Attachment, picture: &Picture, strokes: &[Stroke]) -> Result<(Attachment, Picture), String> {
    if strokes.is_empty() {
        return Ok((original.clone(), picture.clone()));
    }
    use resvg::tiny_skia as sk;
    let mut marks = sk::Pixmap::new(picture.width, picture.height).ok_or("Could not allocate image markers.")?;
    let mut paint = sk::Paint::default();
    paint.set_color_rgba8(255, 0, 0, 255);
    paint.anti_alias = true;
    for stroke in strokes {
        if !stroke.width.is_finite() || stroke.width <= 0.0 || stroke.points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
            return Err("Invalid image marker coordinates.".into());
        }
        let Some(first) = stroke.points.first() else {
            continue;
        };
        if stroke.points.len() == 1 {
            if let Some(path) = sk::PathBuilder::from_circle(first.x, first.y, stroke.width / 2.0) {
                marks.fill_path(&path, &paint, sk::FillRule::Winding, sk::Transform::identity(), None);
            }
        } else {
            let mut builder = sk::PathBuilder::new();
            builder.move_to(first.x, first.y);
            for point in &stroke.points[1..] {
                builder.line_to(point.x, point.y);
            }
            if let Some(path) = builder.finish() {
                marks.stroke_path(
                    &path,
                    &paint,
                    &sk::Stroke { width: stroke.width, line_cap: sk::LineCap::Round, line_join: sk::LineJoin::Round, ..Default::default() },
                    sk::Transform::identity(),
                    None,
                );
            }
        }
    }
    let mut rgba = picture.rgba.as_ref().clone();
    for (pixel, marker) in rgba.chunks_exact_mut(4).zip(marks.data().chunks_exact(4)) {
        let alpha = marker[3] as f32 / 255.0;
        if alpha == 0.0 {
            continue;
        }
        let remaining = (pixel[3] as f32 / 255.0) * (1.0 - alpha);
        let out_alpha = alpha + remaining;
        pixel[0] = ((alpha * 255.0 + pixel[0] as f32 * remaining) / out_alpha).round() as u8;
        pixel[1] = (pixel[1] as f32 * remaining / out_alpha).round() as u8;
        pixel[2] = (pixel[2] as f32 * remaining / out_alpha).round() as u8;
        pixel[3] = (out_alpha * 255.0).round() as u8;
    }
    drop(marks);
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&rgba, picture.width, picture.height, image::ExtendedColorType::Rgba8)
        .map_err(|e| e.to_string())?;
    if bytes.len() > sill_proto::initialize::MAX_INLINE_BYTES {
        return Err("The marked image exceeds 10 MiB. Clear some markers or use a smaller image.".into());
    }
    let original_name = crate::media::name(original);
    let stem = std::path::Path::new(original_name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("image");
    let attachment = Attachment::Inline { name: format!("{stem}.png"), mime: "image/png".into(), data_base64: admin::encode_bytes(&bytes) };
    let preview = Picture {
        handle: iced::widget::image::Handle::from_rgba(picture.width, picture.height, rgba.clone()),
        rgba: Arc::new(rgba),
        width: picture.width,
        height: picture.height,
    };
    Ok((attachment, preview))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Attachment, Picture) {
        let mut bytes = Vec::new();
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut bytes).write_image(&[20, 40, 80, 100].repeat(400), 20, 20, image::ExtendedColorType::Rgba8).unwrap();
        (
            Attachment::Inline { name: "screen.webp".into(), mime: "image/png".into(), data_base64: admin::encode_bytes(&bytes) },
            crate::media::decode(&bytes).unwrap(),
        )
    }

    #[test]
    fn fit_and_source_coordinates_preserve_aspect_after_resize() {
        for image in [Size::new(400.0, 200.0), Size::new(200.0, 400.0)] {
            for bounds in [Size::new(300.0, 300.0), Size::new(800.0, 500.0)] {
                let fitted = fit_rect(image, bounds);
                assert!((fitted.width / fitted.height - image.width / image.height).abs() < 0.001);
                let center = Point::new(fitted.x + fitted.width / 2.0, fitted.y + fitted.height / 2.0);
                assert_eq!(source_point(center, fitted, image), Point::new(image.width / 2.0, image.height / 2.0));
                assert_eq!(source_point(fitted.position(), fitted, image), Point::ORIGIN);
            }
        }
        let fitted = fit_rect(Size::new(400.0, 200.0), Size::new(300.0, 300.0));
        assert!(!fitted.contains(Point::new(20.0, 20.0)), "letterbox space is not drawable");
    }

    #[test]
    fn crossing_boundary_ends_at_segment_intersection() {
        assert_eq!(clipped_end(Point::new(50.0, 50.0), Point::new(150.0, 75.0), Size::new(100.0, 100.0)), Point::new(100.0, 62.5));
        assert_eq!(clipped_end(Point::new(50.0, 50.0), Point::new(-50.0, -150.0), Size::new(100.0, 100.0)), Point::new(25.0, 0.0));
    }

    #[test]
    fn gestures_finish_outside_image_and_on_focus_loss() {
        use canvas::Program;
        let (original, picture) = sample();
        let editor =
            Editor { attachment_id: "a".into(), revision: "revision".into(), original, picture, strokes: vec![], saving: false, drawing: false, error: None };
        let bounds = Rectangle { x: 10.0, y: 20.0, width: 200.0, height: 100.0 };
        let mut state = Gesture::default();
        let press = iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        assert!(editor.update(&mut state, &press, bounds, mouse::Cursor::Available(Point::new(15.0, 30.0))).is_none());
        assert!(state.active.is_none(), "press in letterbox must not start a stroke");
        editor.update(&mut state, &press, bounds, mouse::Cursor::Available(Point::new(110.0, 70.0)));
        assert!((state.active.as_ref().unwrap().width - 0.6).abs() < 0.001);
        let release = iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let (message, _, _) = editor.update(&mut state, &release, bounds, mouse::Cursor::Unavailable).unwrap().into_inner();
        let Some(Message::Annotation(Event::Scoped(revision, event))) = message else { panic!("scoped stroke") };
        assert_eq!(revision, "revision");
        let Event::Stroke(stroke) = *event else { panic!("stroke") };
        assert_eq!(stroke.points, vec![Point::new(10.0, 10.0)], "click creates a dot");
        assert!(state.active.is_none());
        editor.update(&mut state, &press, bounds, mouse::Cursor::Available(Point::new(110.0, 70.0)));
        let event = iced::Event::Mouse(mouse::Event::CursorMoved { position: Point::new(210.0, 95.0) });
        let (message, _, _) = editor.update(&mut state, &event, bounds, mouse::Cursor::Available(Point::new(210.0, 95.0))).unwrap().into_inner();
        let Some(Message::Annotation(Event::Scoped(_, event))) = message else { panic!("scoped stroke") };
        let Event::Stroke(stroke) = *event else { panic!("stroke") };
        assert_eq!(stroke.points.last(), Some(&Point::new(20.0, 12.5)));
        assert!(state.active.is_none(), "leaving image ends gesture");
        editor.update(&mut state, &press, bounds, mouse::Cursor::Available(Point::new(110.0, 70.0)));
        assert!(editor.update(&mut state, &iced::Event::Window(iced::window::Event::Unfocused), bounds, mouse::Cursor::Unavailable).is_some());
        assert!(state.active.is_none());
    }

    #[test]
    fn flatten_preserves_source_resolution_alpha_and_original() {
        let (original, picture) = sample();
        let stroke = Stroke { points: vec![Point::new(10.0, 10.0)], width: 6.0 };
        let (attachment, edited) = render(&original, &picture, &[stroke]).unwrap();
        assert_eq!((edited.width, edited.height), (20, 20));
        assert_eq!(&edited.rgba[..4], &[20, 40, 80, 100]);
        assert_eq!(&picture.rgba[(10 * 20 + 10) * 4..(10 * 20 + 10) * 4 + 4], &[20, 40, 80, 100]);
        assert_eq!(&edited.rgba[(10 * 20 + 10) * 4..(10 * 20 + 10) * 4 + 4], &[255, 0, 0, 255]);
        let Attachment::Inline { name, mime, data_base64 } = attachment else { panic!("inline image") };
        assert_eq!(name, "screen.png");
        assert_eq!(mime, "image/png");
        let decoded = crate::media::decode(&admin::decode_bytes(&data_base64).unwrap()).unwrap();
        assert_eq!(decoded.rgba, edited.rgba);
        let (cleared, _) = render(&original, &picture, &[]).unwrap();
        assert_eq!(cleared, original);
    }

    #[test]
    fn freehand_is_clipped_and_rejects_invalid_coordinates() {
        let (original, picture) = sample();
        let stroke = Stroke { points: vec![Point::new(0.0, 0.0), Point::new(20.0, 20.0)], width: 4.0 };
        let (_, edited) = render(&original, &picture, &[stroke]).unwrap();
        assert_eq!(&edited.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&edited.rgba[(19 * 20) * 4..(19 * 20) * 4 + 4], &[20, 40, 80, 100]);
        assert!(render(&original, &picture, &[Stroke { points: vec![Point::ORIGIN], width: f32::NAN }]).is_err());
    }

    #[test]
    fn canvas_marks_align_with_export_inside_letterboxed_image() {
        let (original, picture) = sample();
        let editor = Editor {
            attachment_id: "a".into(),
            revision: "r".into(),
            original,
            picture,
            strokes: vec![Stroke { points: vec![Point::new(10.0, 10.0)], width: 4.0 }],
            saving: false,
            drawing: false,
            error: None,
        };
        // A square image in a wide canvas begins 50 logical pixels from its left edge.
        let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (200.0, 100.0), surface(&editor));
        let snapshot = ui.snapshot(&style::theme()).unwrap();
        let dir = std::env::temp_dir().join(sill_client::request_id("annotation-preview"));
        snapshot.matches_image(dir.join("preview")).unwrap();
        let png = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let preview = image::open(png).unwrap().to_rgba8();
        std::fs::remove_dir_all(dir).unwrap();
        // Simulator renders at scale 2: the red dot must be at (200, 100), not (100, 100).
        assert_eq!(preview.get_pixel(200, 100).0, [255, 0, 0, 255]);
        assert_ne!(preview.get_pixel(100, 100).0, [255, 0, 0, 255]);
        let (_, saved) = render(&editor.original, &editor.picture, &editor.strokes).unwrap();
        assert_eq!(&saved.rgba[(10 * 20 + 10) * 4..(10 * 20 + 10) * 4 + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn pen_cursor_hides_os_pointer_and_tracks_image_hover() {
        use canvas::Program;
        let (original, picture) = sample();
        let mut editor =
            Editor { attachment_id: "a".into(), revision: "r".into(), original, picture, strokes: vec![], saving: false, drawing: false, error: None };
        let bounds = Rectangle { x: 0.0, y: 0.0, width: 200.0, height: 100.0 };
        let mut state = Gesture::default();
        assert_eq!(
            editor.mouse_interaction(&state, bounds, mouse::Cursor::Available(Point::new(10.0, 50.0))),
            mouse::Interaction::default(),
            "letterbox keeps default cursor"
        );
        assert_eq!(
            editor.mouse_interaction(&state, bounds, mouse::Cursor::Available(Point::new(100.0, 50.0))),
            mouse::Interaction::Hidden,
            "image area hides OS cursor for the drawn pen pointer"
        );
        let moved = iced::Event::Mouse(mouse::Event::CursorMoved { position: Point::new(100.0, 50.0) });
        assert!(editor.update(&mut state, &moved, bounds, mouse::Cursor::Available(Point::new(100.0, 50.0))).is_some());
        assert!(state.cursor_inside);
        // Leaving into letterbox requests one redraw so the pen disappears.
        assert!(
            editor
                .update(
                    &mut state,
                    &iced::Event::Mouse(mouse::Event::CursorMoved { position: Point::new(10.0, 50.0) }),
                    bounds,
                    mouse::Cursor::Available(Point::new(10.0, 50.0))
                )
                .is_some()
        );
        assert!(!state.cursor_inside);
        assert!(
            editor
                .update(
                    &mut state,
                    &iced::Event::Mouse(mouse::Event::CursorMoved { position: Point::new(5.0, 50.0) }),
                    bounds,
                    mouse::Cursor::Available(Point::new(5.0, 50.0))
                )
                .is_none()
        );
        editor.saving = true;
        assert_eq!(editor.mouse_interaction(&state, bounds, mouse::Cursor::Available(Point::new(100.0, 50.0))), mouse::Interaction::default());
    }
}
