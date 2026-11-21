//! Keyboard operation (docs/silli-gui.md, implementation direction 6): every control takes the
//! keyboard and shows that it has it, and the window handles session and layout shortcuts.
//!
//! iced 0.14 lets only text inputs and editors take focus. [`focus`] puts any control into
//! iced's own focus traversal, so Tab and Shift+Tab move through the window in reading order —
//! text fields included — Enter or Space presses the focused control, and a ring shows where the
//! keyboard is. Moving focus scrolls the control into sight ([`step`]). A press of the pointer
//! hides the ring until the keyboard moves focus again.
use std::rc::Rc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::operation::focusable::{self, Focusable};
use iced::advanced::widget::operation::scrollable::{self as scroll, AbsoluteOffset, Scrollable};
use iced::advanced::widget::operation::{Operation, Outcome, TextInput};
use iced::advanced::widget::{Id, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay};
use iced::keyboard::{self, Key, Modifiers, key::Named, key::Physical};
use iced::widget::Button;
use iced::{Border, Color, Element, Event, Length, Rectangle, Renderer, Size, Task, Theme, Vector};

use crate::Message;
use crate::style::P;

/// What a focused control makes of a key besides Enter and Space.
type OnKey<'a, M> = Rc<dyn Fn(&Key, Modifiers) -> Option<M> + 'a>;
/// What a wrapper makes of a press before its content sees it; with the physical key.
type Before<'a, M> = Rc<dyn Fn(&Key, Physical, Modifiers) -> Option<M> + 'a>;

/// A control in the window's focus traversal: Enter or Space sends `on_activate`; other keys go
/// to `on_key` while it has the keyboard.
pub struct Focus<'a, M> {
    content: Element<'a, M>,
    on_activate: Option<M>,
    on_key: Option<OnKey<'a, M>>,
    before_children: Option<Before<'a, M>>,
}

#[derive(Default)]
struct State {
    focused: bool,
}

impl Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
    }
}

pub fn focus<'a, M>(content: impl Into<Element<'a, M>>) -> Focus<'a, M> {
    Focus { content: content.into(), on_activate: None, on_key: None, before_children: None }
}

impl<'a, M> Focus<'a, M> {
    pub fn on_activate(mut self, message: M) -> Self {
        self.on_activate = Some(message);
        self
    }

    pub fn on_key(mut self, keys: impl Fn(&Key, Modifiers) -> Option<M> + 'a) -> Self {
        self.on_key = Some(Rc::new(keys));
        self
    }
}

/// Reserved window shortcuts run before focused text fields and the shell. This wrapper
/// does not join focus traversal; ordinary editing keys go straight to the child.
#[cfg(test)]
pub fn intercept_shortcuts<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let mut wrapper = focus(content);
    wrapper.before_children = Some(Rc::new(|key, _, modifiers| window_message(key, modifiers)));
    wrapper.into()
}

#[cfg(test)]
pub fn configured_shortcuts<'a>(content: impl Into<Element<'a, Message>>, bindings: &'a crate::shortcuts::Bindings) -> Element<'a, Message> {
    window_shortcuts(content, bindings, false, false, false)
}

/// The window's shortcuts, taken before any control. While Settings records a shortcut
/// (`recording`) every press goes to the recorder instead: no shortcut fires, no field types.
pub fn window_shortcuts<'a>(
    content: impl Into<Element<'a, Message>>,
    bindings: &'a crate::shortcuts::Bindings,
    find_shown: bool,
    terminal_focused: bool,
    recording: bool,
) -> Element<'a, Message> {
    let mut wrapper = focus(content);
    wrapper.before_children = Some(Rc::new(move |key, physical, modifiers| {
        if recording {
            return Some(Message::Settings(crate::settings_ui::Event::ShortcutKey(key.clone(), physical, modifiers)));
        }
        if find_shown {
            match key.as_ref() {
                Key::Named(Named::F3) if !modifiers.command() && !modifiers.alt() => {
                    return Some(Message::Find(if modifiers.shift() { crate::find::Event::Prev } else { crate::find::Event::Next }));
                }
                Key::Character(c) if modifiers.alt() && !modifiers.command() && !modifiers.shift() && c.eq_ignore_ascii_case("c") => {
                    return Some(Message::Find(crate::find::Event::ToggleCase));
                }
                _ => {}
            }
        }
        let message = crate::shortcuts::message_at(bindings, key, physical, modifiers)?;
        if terminal_focused && matches!(message, Message::Find(crate::find::Event::Open)) {
            return None;
        }
        Some(message)
    }));
    wrapper.into()
}

/// Wraps the Find bar's query input so `Enter` sends `Next` and `Shift+Enter` sends `Prev`
/// while the query input has focus, without joining focus traversal itself.
pub fn find_input<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(FindInput { content: content.into() })
}

struct FindInput<'a> {
    content: Element<'a, Message>,
}

struct ChildFocused(bool);

impl Operation for ChildFocused {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        self.0 |= state.is_focused();
    }
}

impl Widget<Message, Theme, Renderer> for FindInput<'_> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
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
        if let Event::Keyboard(keyboard::Event::KeyPressed { key: Key::Named(Named::Enter), modifiers, .. }) = event
            && !modifiers.command()
            && !modifiers.alt()
        {
            let mut focused = ChildFocused(false);
            self.content.as_widget_mut().operate(tree, layout, renderer, &mut focused);
            if focused.0 {
                shell.publish(Message::Find(if modifiers.shift() { crate::find::Event::Prev } else { crate::find::Event::Next }));
                shell.capture_event();
                return;
            }
        }
        self.content.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
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
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &Renderer) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

/// Annotation shortcuts take precedence over hidden prompt/file editors while the canvas is shown.
pub fn annotation_shortcuts<'a>(content: impl Into<Element<'a, Message>>, active: bool) -> Element<'a, Message> {
    // Keep the same wrapper and widget tree when the editor opens and closes.
    let mut wrapper = focus(content);
    wrapper.before_children = Some(Rc::new(move |key, _, modifiers| active.then(|| annotation_key(key, modifiers)).flatten()));
    wrapper.into()
}

fn annotation_key(key: &Key, modifiers: Modifiers) -> Option<Message> {
    if !modifiers.command() || modifiers.shift() || modifiers.alt() {
        return None;
    }
    let Key::Character(key) = key.as_ref() else {
        return None;
    };
    if key.eq_ignore_ascii_case("s") {
        Some(Message::Annotation(crate::annotation::Event::Save))
    } else if key.eq_ignore_ascii_case("z") {
        Some(Message::Annotation(crate::annotation::Event::Undo))
    } else {
        None
    }
}

/// The platform's own modifier for commands: Cmd on macOS, Ctrl elsewhere (iced's `command()`).
/// Shortcuts written `Ctrl+…` mean it, and on macOS say `Cmd+…`.
pub const PRIMARY: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

/// A text naming shortcuts as `Ctrl+…`, in the platform's words.
pub fn label(text: &str) -> String {
    if cfg!(target_os = "macos") { text.replace("Ctrl+", "Cmd+") } else { text.to_string() }
}

/// A button the keyboard reaches too: Tab focuses it, Enter or Space presses it.
pub fn press<'a>(button: Button<'a, Message>, message: Message) -> Element<'a, Message> {
    focus(button.on_press(message.clone())).on_activate(message).into()
}

/// [`press`] for a button that may be disabled. A disabled button is skipped by Tab.
pub fn press_maybe<'a>(button: Button<'a, Message>, message: Option<Message>) -> Element<'a, Message> {
    match message {
        Some(message) => press(button, message),
        None => button.into(),
    }
}

impl<M: Clone> Widget<M, Theme, Renderer> for Focus<'_, M> {
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
        if self.before_children.is_none() {
            operation.focusable(None, layout.bounds(), tree.state.downcast_mut::<State>());
        }
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
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        if let Event::Keyboard(keyboard::Event::KeyPressed { key, physical_key, modifiers, .. }) = event
            && let Some(message) = self.before_children.as_ref().and_then(|on| on(key, *physical_key, *modifiers))
        {
            shell.publish(message);
            shell.capture_event();
            return;
        }
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) if state.focused => {
                let pressed = matches!(key.as_ref(), Key::Named(Named::Enter | Named::Space)) && !modifiers.control() && !modifiers.alt();
                let message = pressed.then(|| self.on_activate.clone()).flatten().or_else(|| self.on_key.as_ref().and_then(|keys| keys(key, *modifiers)));
                if let Some(message) = message {
                    shell.publish(message);
                    shell.capture_event();
                    return;
                }
            }
            // the pointer took over: no ring until the keyboard moves focus again
            Event::Mouse(mouse::Event::ButtonPressed(_)) if state.focused => {
                state.focused = false;
            }
            _ => {}
        }
        self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
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
        use iced::advanced::Renderer as _;
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
        if tree.state.downcast_ref::<State>().focused {
            renderer.fill_quad(
                Quad { bounds: layout.bounds(), border: Border { color: P.accent, width: 2.0, radius: crate::style::CONTROL.into() }, ..Quad::default() },
                Color::TRANSPARENT,
            );
        }
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &Renderer) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, M: Clone + 'a> From<Focus<'a, M>> for Element<'a, M> {
    fn from(focus: Focus<'a, M>) -> Self {
        Element::new(focus)
    }
}

/// Tab (`forward`) or Shift+Tab: the next or previous control takes the keyboard, scrolled into
/// sight.
pub fn step(forward: bool) -> Task<Message> {
    let inner: Box<dyn Operation> = if forward { Box::new(focusable::focus_next()) } else { Box::new(focusable::focus_previous()) };
    effect(AndReveal { inner })
}

/// A task that only runs `operation`, as iced's own focus operations are built: no output, so
/// nothing waits on one.
pub(crate) fn effect(operation: impl Operation + 'static) -> Task<Message> {
    iced_runtime::task::effect(iced_runtime::Action::widget(operation))
}

/// Focuses the text input `id` and selects all of its text.
pub fn focus_and_select_all(id: &'static str) -> Task<Message> {
    use iced::advanced::widget::operation::text_input;
    Task::batch([iced::widget::operation::focus(id), effect(text_input::select_all(Id::new(id)))])
}

/// Runs `inner` (and whatever it chains), then [`Reveal`].
struct AndReveal {
    inner: Box<dyn Operation>,
}

impl Operation for AndReveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        self.inner.traverse(operate);
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        self.inner.container(id, bounds);
    }
    fn scrollable(&mut self, id: Option<&Id>, bounds: Rectangle, content_bounds: Rectangle, translation: Vector, state: &mut dyn Scrollable) {
        self.inner.scrollable(id, bounds, content_bounds, translation, state);
    }
    fn focusable(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        self.inner.focusable(id, bounds, state);
    }
    fn text_input(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn TextInput) {
        self.inner.text_input(id, bounds, state);
    }
    fn text(&mut self, id: Option<&Id>, bounds: Rectangle, text: &str) {
        self.inner.text(id, bounds, text);
    }
    fn custom(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        self.inner.custom(id, bounds, state);
    }
    fn finish(&self) -> Outcome<()> {
        match self.inner.finish() {
            Outcome::Chain(next) => Outcome::Chain(Box::new(AndReveal { inner: next })),
            _ => Outcome::Chain(Box::new(Reveal::default())),
        }
    }
}

/// A scrollable on the way down the tree: its viewport and how far it is scrolled.
struct Frame {
    id: Option<Id>,
    bounds: Rectangle,
    translation: Vector,
}

/// Finds the focused control and, when it lies outside the innermost named scrollable around it,
/// scrolls that one so the control is in sight with a margin.
#[derive(Default)]
struct Reveal {
    entered: Option<Frame>,
    frames: Vec<Frame>,
    target: Option<(Id, f32)>,
}

impl Operation for Reveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        // a scrollable reports itself, then traverses its content (untranslated)
        match self.entered.take() {
            Some(frame) => {
                self.frames.push(frame);
                operate(self);
                self.frames.pop();
            }
            None => operate(self),
        }
    }

    fn scrollable(&mut self, id: Option<&Id>, bounds: Rectangle, _content_bounds: Rectangle, translation: Vector, _state: &mut dyn Scrollable) {
        self.entered = Some(Frame { id: id.cloned(), bounds, translation });
    }

    fn focusable(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        if !state.is_focused() {
            return;
        }
        let Some(frame) = self.frames.iter().rev().find(|f| f.id.is_some()) else {
            return;
        };
        const MARGIN: f32 = 24.0;
        let top = frame.bounds.y + frame.translation.y;
        let bottom = top + frame.bounds.height;
        let offset = if bounds.y < top {
            bounds.y - frame.bounds.y - MARGIN
        } else if bounds.y + bounds.height > bottom {
            bounds.y + bounds.height - frame.bounds.y - frame.bounds.height + MARGIN
        } else {
            return;
        };
        self.target = frame.id.clone().map(|id| (id, offset.max(0.0)));
    }

    fn finish(&self) -> Outcome<()> {
        match &self.target {
            Some((id, y)) => Outcome::Chain(Box::new(scroll::scroll_to(id.clone(), AbsoluteOffset { x: None, y: Some(*y) }))),
            None => Outcome::None,
        }
    }
}

/// PageUp / PageDown: the first of `ids` that is on screen, by most of its height.
pub fn page(ids: &[&'static str], down: bool) -> Task<Message> {
    struct Page {
        ids: Vec<Id>,
        down: bool,
        done: bool,
    }
    impl Operation for Page {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn scrollable(&mut self, id: Option<&Id>, bounds: Rectangle, content_bounds: Rectangle, _translation: Vector, state: &mut dyn Scrollable) {
            if self.done || !id.is_some_and(|id| self.ids.contains(id)) {
                return;
            }
            self.done = true;
            let step = bounds.height * 0.85;
            state.scroll_by(AbsoluteOffset { x: 0.0, y: if self.down { step } else { -step } }, bounds, content_bounds);
        }
    }
    effect(Page { ids: ids.iter().copied().map(Id::new).collect(), down, done: false })
}

/// New text arrived while `id` followed its tail: scroll it to its end, unless it has been
/// scrolled up from `from` (the offset the view last knew) while this was on its way. The
/// person's scroll away from the tail then stands, and is not pulled back.
pub fn follow_end(id: &'static str, from: f32) -> Task<Message> {
    struct Follow {
        id: Id,
        from: f32,
    }
    impl Operation for Follow {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn scrollable(&mut self, id: Option<&Id>, _bounds: Rectangle, _content_bounds: Rectangle, translation: Vector, state: &mut dyn Scrollable) {
            if id == Some(&self.id) && translation.y >= self.from - 1.0 {
                state.snap_to(scroll::RelativeOffset { x: None, y: Some(1.0) });
            }
        }
    }
    effect(Follow { id: Id::new(id), from })
}

/// Window shortcuts shared by the root, editors and shell; word-selection keys stay local.
#[cfg(test)]
fn window_message(key: &Key, modifiers: Modifiers) -> Option<Message> {
    crate::shortcuts::message(&crate::shortcuts::Bindings::new(), key, modifiers)
}

/// What a key no control took means to the window.
#[cfg(test)]
pub fn shortcut(event: keyboard::Event) -> Option<Message> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };
    if let Some(message) = window_message(&key, modifiers) {
        return Some(message);
    }
    navigation(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    })
}

pub fn navigation(event: keyboard::Event) -> Option<Message> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };
    let (ctrl, shift) = (modifiers.command(), modifiers.shift());
    Some(match key.as_ref() {
        Key::Named(Named::Tab) if !ctrl && !modifiers.alt() => Message::Focus(!shift),
        Key::Character("s") if ctrl => Message::Files(crate::files::Action::SaveFile),
        Key::Character("q") if ctrl => Message::Quit,
        Key::Named(Named::Escape) => Message::Escape,
        Key::Named(Named::PageUp) if !ctrl => Message::Page(false),
        Key::Named(Named::PageDown) if !ctrl => Message::Page(true),
        Key::Named(Named::End) if ctrl => Message::Latest,
        _ => return None,
    })
}

/// Prompt paging; configured window shortcuts are handled by the root wrapper.
/// Ctrl+Shift+Left/Right stays with the editor for selecting by words.
/// Escape needs nothing: an editor lets it through. So does Tab, except in the prompt and the
/// instructions, which indent a list item with it and step the focus themselves otherwise.
pub fn from_text(key: &Key, modifiers: Modifiers, paging: bool) -> Option<Message> {
    match key.as_ref() {
        Key::Named(Named::PageUp) if paging && !modifiers.command() => Some(Message::Page(false)),
        Key::Named(Named::PageDown) if paging && !modifiers.command() => Some(Message::Page(true)),
        _ => None,
    }
}

#[cfg(test)]
mod annotation_tests {
    use super::*;

    #[test]
    fn annotation_keys_override_focused_controls_only_while_editor_is_visible() {
        for active in [true, false] {
            let content = iced::widget::text_editor::Content::with_text("draft");
            let input = iced::widget::text_editor(&content)
                .id("annotation-shortcut-input")
                .on_action(|action| crate::session::message("demo", crate::session::Event::Prompt(action)))
                .key_binding(crate::ui::reading_keys);
            let view = annotation_shortcuts(input, active);
            let mut ui = iced_test::Simulator::with_size(crate::snapshots::settings(), (400.0, 100.0), view);
            ui.click(Id::new("annotation-shortcut-input")).unwrap();
            for character in ["s", "z"] {
                let key = Key::Character(character.into());
                let _ = ui.simulate(vec![
                    Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::COMMAND)),
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key,
                        physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
                        location: keyboard::Location::Standard,
                        modifiers: Modifiers::COMMAND,
                        text: None,
                        repeat: false,
                    }),
                ]);
            }
            let messages: Vec<_> = ui.into_messages().filter(|message| !matches!(message, Message::Session(_, crate::session::Event::Prompt(_)))).collect();
            if active {
                assert!(matches!(
                    messages.as_slice(),
                    [Message::Annotation(crate::annotation::Event::Save), Message::Annotation(crate::annotation::Event::Undo),]
                ));
            } else {
                assert!(messages.iter().any(|m| matches!(m, Message::Files(crate::files::Action::SaveFile))));
                assert!(!messages.iter().any(|m| matches!(m, Message::Annotation(_))));
            }
        }
    }
}
