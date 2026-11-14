//! A right-click menu over a control, in the agent controls' treatment: the same overlay list
//! the pick lists use (`iced_widget::overlay::menu`), floating over the window rather than
//! taking room in the layout. A right press on the content opens it; a choice, a press
//! anywhere else or another press on the content closes it. Whether it is open is the caller's
//! state (so Escape and a tab change can close it); this widget only reports presses.
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::operation::Operation;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay};
use iced::widget::overlay::menu::{self, Menu};
use iced::{Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector};

use crate::Message;
use crate::style;

/// Room on each side of a choice, as in the agent controls' menus (`ui.rs`).
const SIDE: f32 = 14.0;
/// The gap between the control and the list.
const GAP: f32 = 6.0;

pub struct Popup<'a> {
    content: Element<'a, Message>,
    open: bool,
    choices: Vec<String>,
    on_choice: Box<dyn Fn(usize) -> Message + 'a>,
    /// Sent on a right press over the content while closed, and on any press while open.
    on_toggle: Message,
    class: menu::StyleFn<'a, Theme>,
}

#[derive(Default)]
struct State {
    menu: menu::State,
    hovered: Option<usize>,
}

/// `content` with a right-click menu of `choices`; `on_choice` gets the index chosen.
pub fn popup<'a>(
    content: impl Into<Element<'a, Message>>,
    open: bool,
    choices: Vec<String>,
    on_choice: impl Fn(usize) -> Message + 'a,
    on_toggle: Message,
) -> Popup<'a> {
    Popup {
        content: content.into(),
        open,
        choices,
        on_choice: Box::new(on_choice),
        on_toggle,
        class: Box::new(style::control_menu),
    }
}

impl Widget<Message, Theme, Renderer> for Popup<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::ButtonPressed(button)) = event {
            // open: the overlay closes on any press (inside: a choice; outside: dismissed); this
            // is the fallback should one reach here
            if self.open {
                shell.publish(self.on_toggle.clone());
                shell.capture_event();
                return;
            }
            if *button == mouse::Button::Right && cursor.is_over(layout.bounds()) {
                tree.state.downcast_mut::<State>().hovered = None;
                shell.publish(self.on_toggle.clone());
                shell.capture_event();
                return;
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        if !self.open {
            return self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation);
        }
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let choices = &self.choices;
        let on_choice = &self.on_choice;
        let dismiss = self.on_toggle.clone();
        // the list reads as a menu: as wide as its longest choice, in the controls' menu text
        let width = choices
            .iter()
            .map(|c| crate::ui::text_width(c, style::SMALL))
            .fold(0.0, f32::max)
            + 2.0 * SIDE
            + 2.0;
        let menu = Menu::new(
            &mut state.menu,
            choices,
            &mut state.hovered,
            move |chosen: String| {
                let index = choices.iter().position(|c| *c == chosen).unwrap_or(0);
                on_choice(index)
            },
            None,
            &self.class,
        )
        .width(width.max(bounds.width))
        .padding([6.0, SIDE])
        .text_size(style::SMALL)
        // a press outside the list closes it, wherever it lands
        .on_dismiss(move || dismiss.clone());
        Some(menu.overlay(
            layout.position() + translation - Vector::new(0.0, GAP),
            *viewport,
            bounds.height + 2.0 * GAP,
            Length::Shrink,
        ))
    }
}

impl<'a> From<Popup<'a>> for Element<'a, Message> {
    fn from(popup: Popup<'a>) -> Self {
        Element::new(popup)
    }
}
