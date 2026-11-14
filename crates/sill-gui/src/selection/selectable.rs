//! Selectable text leaves: plain and rich text that draw the highlight the selection [`Area`]
//! asks for, and report their paragraph to it through `Operation::custom`.
//!
//! Forked from `iced_selection` 0.5.0 (`src/text.rs`, `src/text/rich.rs`, MIT). What stays: layout
//! and paragraph caching, span decorations (inline-code highlight, underline, strikethrough) and
//! link hover/click. What goes: all mouse and keyboard selection handling — a leaf does not select
//! on its own; the enclosing [`Area`] owns one selection for all of its leaves.
//!
//! [`Area`]: super::Area
use iced::advanced::graphics::text::Paragraph;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::text::{self as core_text, Difference, Paragraph as _, Span};
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell};
use iced::alignment;
use iced::mouse;
use iced::touch;
use iced::widget::text::{Alignment, Fragment, IntoFragment, LineHeight, Shaping, Wrapping};
use iced::{Element, Event, Font, Length, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector};
use iced_selection::text::{Style, StyleFn};

use super::model::Selectable;

enum Content<'a, Link> {
    Plain(Fragment<'a>),
    Rich(Box<dyn AsRef<[Span<'a, Link, Font>]> + 'a>),
}

/// Selectable plain or rich text; it selects only inside a selection area.
pub struct Text<'a, Link, Message>
where
    Link: Clone + 'static,
{
    content: Content<'a, Link>,
    size: Option<Pixels>,
    width: Length,
    font: Option<Font>,
    align_x: Alignment,
    style: StyleFn<'a, Theme>,
    on_link_click: Option<Box<dyn Fn(Link) -> Message + 'a>>,
    on_link_copy: Option<Box<dyn Fn(Link) -> Message + 'a>>,
}

/// Selectable plain text.
pub fn text<'a, Message>(content: impl IntoFragment<'a>) -> Text<'a, (), Message> {
    Text::with_content(Content::Plain(content.into_fragment()))
}

/// Selectable rich text.
pub fn rich_text<'a, Link, Message>(
    spans: impl AsRef<[Span<'a, Link, Font>]> + 'a,
) -> Text<'a, Link, Message>
where
    Link: Clone + 'static,
{
    Text::with_content(Content::Rich(Box::new(spans)))
}

impl<'a, Link, Message> Text<'a, Link, Message>
where
    Link: Clone + 'static,
{
    fn with_content(content: Content<'a, Link>) -> Self {
        Self {
            content,
            size: None,
            width: Length::Shrink,
            font: None,
            align_x: Alignment::Default,
            style: Box::new(iced_selection::text::default),
            on_link_click: None,
            on_link_copy: None,
        }
    }

    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into());
        self
    }

    pub fn font(mut self, font: impl Into<Font>) -> Self {
        self.font = Some(font.into());
        self
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn align_x(mut self, alignment: impl Into<Alignment>) -> Self {
        self.align_x = alignment.into();
        self
    }

    /// The text colour and the selection colour (`iced_selection`'s style, so
    /// `style::selectable` works unchanged).
    pub fn style(mut self, style: impl Fn(&Theme) -> Style + 'a) -> Self {
        self.style = Box::new(style);
        self
    }

    pub fn on_link_click(mut self, on_link_click: impl Fn(Link) -> Message + 'a) -> Self {
        self.on_link_click = Some(Box::new(on_link_click));
        self
    }

    /// Copy a link with the secondary pointer button.
    pub fn on_link_copy(mut self, on_link_copy: impl Fn(Link) -> Message + 'a) -> Self {
        self.on_link_copy = Some(Box::new(on_link_copy));
        self
    }

    fn spans(&self) -> &[Span<'a, Link, Font>] {
        match &self.content {
            Content::Plain(_) => &[],
            Content::Rich(spans) => spans.as_ref().as_ref(),
        }
    }
}

enum Last<Link> {
    Nothing,
    Plain(String),
    Rich(Vec<Span<'static, Link, Font>>),
}

struct State<Link> {
    last: Last<Link>,
    selectable: Selectable,
    span_pressed: Option<usize>,
    hovered_link: Option<usize>,
}

impl<Link, Message> Widget<Message, Theme, Renderer> for Text<'_, Link, Message>
where
    Link: Clone + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Link>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Link> {
            last: Last::Nothing,
            selectable: Selectable::default(),
            span_pressed: None,
            hovered_link: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Shrink,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        use iced::advanced::text::Renderer as _;

        let state = tree.state.downcast_mut::<State<Link>>();
        layout::sized(limits, self.width, Length::Shrink, |limits| {
            let bounds = limits.max();
            let size = self.size.unwrap_or_else(|| renderer.default_size());
            let font = self.font.unwrap_or_else(|| renderer.default_font());
            let shape = |shaping| core_text::Text {
                content: (),
                bounds,
                size,
                line_height: LineHeight::default(),
                font,
                align_x: self.align_x,
                align_y: alignment::Vertical::Top,
                shaping,
                wrapping: Wrapping::default(),
            };
            match &self.content {
                Content::Plain(content) => {
                    let text = shape(Shaping::default()).with_content(content.as_ref());
                    let same = matches!(&state.last, Last::Plain(last) if last == content);
                    if !same {
                        state.last = Last::Plain(content.to_string());
                        state.selectable.set_paragraph(Paragraph::with_text(text));
                    } else {
                        match state
                            .selectable
                            .paragraph()
                            .compare(shape(Shaping::default()))
                        {
                            Difference::None => {}
                            Difference::Bounds => state.selectable.resize(bounds),
                            Difference::Shape => {
                                state.selectable.set_paragraph(Paragraph::with_text(text));
                            }
                        }
                    }
                }
                Content::Rich(spans) => {
                    let spans = spans.as_ref().as_ref();
                    // rich text is always shaped in full, as iced's
                    let text = shape(Shaping::Advanced).with_content(spans);
                    let same = matches!(&state.last, Last::Rich(last) if last.as_slice() == spans);
                    if !same {
                        state.last =
                            Last::Rich(spans.iter().cloned().map(Span::to_static).collect());
                        state.selectable.set_paragraph(Paragraph::with_spans(text));
                    } else {
                        match state
                            .selectable
                            .paragraph()
                            .compare(shape(Shaping::Advanced))
                        {
                            Difference::None => {}
                            Difference::Bounds => state.selectable.resize(bounds),
                            Difference::Shape => {
                                state.selectable.set_paragraph(Paragraph::with_spans(text));
                            }
                        }
                    }
                }
            }
            state.selectable.paragraph().min_bounds()
        })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        use iced::advanced::text::Renderer as _;

        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let state = tree.state.downcast_ref::<State<Link>>();
        let style = (self.style)(theme);
        let origin = state.selectable.origin(bounds);
        let translation = origin - Point::ORIGIN;

        // span decorations, as iced's and iced_selection's rich text draw them
        for (index, span) in self.spans().iter().enumerate() {
            let is_hovered_link = self.on_link_click.is_some() && Some(index) == state.hovered_link;
            if span.highlight.is_none()
                && !span.underline
                && !span.strikethrough
                && !is_hovered_link
            {
                continue;
            }
            let regions = state.selectable.paragraph().span_bounds(index);
            if let Some(highlight) = span.highlight {
                for region in &regions {
                    let region = Rectangle::new(
                        region.position() - Vector::new(span.padding.left, span.padding.top),
                        region.size() + Size::new(span.padding.x(), span.padding.y()),
                    );
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: region + translation,
                            border: highlight.border,
                            ..renderer::Quad::default()
                        },
                        highlight.background,
                    );
                }
            }
            if span.underline || span.strikethrough || is_hovered_link {
                let size = span.size.or(self.size).unwrap_or(renderer.default_size());
                let line_height = span.line_height.unwrap_or_default().to_absolute(size);
                let color = span.color.or(style.color).unwrap_or(defaults.text_color);
                let baseline =
                    translation + Vector::new(0.0, size.0 + (line_height.0 - size.0) / 2.0);
                let underline = span.underline || is_hovered_link;
                let offsets: &[f32] = match (underline, span.strikethrough) {
                    (true, true) => &[size.0 * 0.08, size.0 / 2.0],
                    (true, false) => &[size.0 * 0.08],
                    (false, true) => &[size.0 / 2.0],
                    (false, false) => &[],
                };
                for region in &regions {
                    for offset in offsets {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    region.position() + baseline - Vector::new(0.0, *offset),
                                    Size::new(region.width, 1.0),
                                ),
                                ..renderer::Quad::default()
                            },
                            color,
                        );
                    }
                }
            }
        }

        // this leaf's part of the area's selection, under the glyphs
        for rect in state.selectable.highlight_rects().iter() {
            if let Some(visible) = bounds.intersection(&(*rect + translation)) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: visible,
                        ..renderer::Quad::default()
                    },
                    style.selection,
                );
            }
        }

        renderer.fill_paragraph(
            state.selectable.paragraph(),
            origin,
            style.color.unwrap_or(defaults.text_color),
            *viewport,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if self.on_link_click.is_none() {
            return;
        }
        let state = tree.state.downcast_mut::<State<Link>>();
        let bounds = layout.bounds();
        let hovered_before = state.hovered_link;
        let origin = state.selectable.origin(bounds);
        state.hovered_link = cursor
            .position_over(bounds)
            .and_then(|position| {
                state
                    .selectable
                    .paragraph()
                    .hit_span(position - (origin - Point::ORIGIN))
            })
            .filter(|span| self.spans().get(*span).is_some_and(|s| s.link.is_some()));

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                if let Some(span) = state.hovered_link
                    && let Some(link) = self.spans().get(span).and_then(|s| s.link.clone())
                    && let Some(copy) = &self.on_link_copy
                {
                    shell.publish(copy(link));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if state.hovered_link.is_some() {
                    state.span_pressed = state.hovered_link;
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. }) => {
                if let Some(span) = state.span_pressed.take()
                    && Some(span) == state.hovered_link
                    && let Some(link) = self.spans().get(span).and_then(|s| s.link.clone())
                    && let Some(on_link_click) = &self.on_link_click
                {
                    shell.publish(on_link_click(link));
                }
            }
            Event::Touch(touch::Event::FingerLost { .. }) => state.span_pressed = None,
            _ => {}
        }

        if state.hovered_link != hovered_before {
            shell.request_redraw();
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State<Link>>();
        // as iced's own text does, so selectors in tests find leaves by their text
        operation.text(None, layout.bounds(), state.selectable.text());
        operation.custom(None, layout.bounds(), &mut state.selectable);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State<Link>>();
        if state.hovered_link.is_some() {
            mouse::Interaction::Pointer
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Link, Message> From<Text<'a, Link, Message>> for Element<'a, Message>
where
    Link: Clone + 'static,
    Message: 'a,
{
    fn from(text: Text<'a, Link, Message>) -> Self {
        Element::new(text)
    }
}
