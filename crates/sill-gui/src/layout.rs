//! How the window is divided (docs/sill-gui.md, *Window and layout modes*). The window's size is
//! the window manager's or the person's, never the shelf's: the drawer keeps a width in pixels and
//! the conversation takes the rest; when the window cannot fit both, the drawer opens over the
//! conversation instead of beside it. Prose is held to a reading measure, except in *full
//! conversation* (Ctrl+Shift+E), which also puts the drawer away until it is left.
use iced::{Event, Subscription, mouse};

use crate::Message;

/// The drawer's width until the person drags it.
pub const DRAWER_DEFAULT: f32 = 280.0;
pub const DRAWER_MIN: f32 = 160.0;
/// What the conversation keeps at least with the drawer beside it.
pub const MAIN_MIN: f32 = 360.0;
/// What stays visible of the conversation beside a drawer over it: a press there closes it.
pub const UNCOVERED: f32 = 48.0;
/// The conversation's reading measure, as the shelf's (`shelf/conversation.rs`).
pub const MEASURE: f32 = 800.0;
/// The grip between the conversation and the drawer.
pub const HANDLE: f32 = 7.0;

#[derive(Debug, Default)]
pub struct Layout {
    /// The width the person dragged the drawer to; for this run of the window.
    pub drawer: Option<f32>,
    /// Full conversation: no reading measure, the drawer put away.
    pub full: bool,
    /// The drawer's edge is being dragged.
    pub dragging: bool,
}

/// Where the drawer goes in a window `total` wide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drawer {
    Hidden,
    Beside(f32),
    Over(f32),
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    ToggleFull,
    DragStart,
    /// The pointer's x in the window while dragging.
    DragTo(f32),
    DragEnd,
}

impl Layout {
    fn wanted(&self) -> f32 {
        self.drawer.unwrap_or(DRAWER_DEFAULT).max(DRAWER_MIN)
    }

    /// The drawer's place: beside the conversation when both fit, else over it.
    pub fn drawer(&self, shown: bool, total: f32) -> Drawer {
        if !shown || self.full {
            return Drawer::Hidden;
        }
        let wanted = self.wanted();
        if total - wanted - HANDLE >= MAIN_MIN {
            Drawer::Beside(wanted)
        } else {
            Drawer::Over(wanted.min(total - UNCOVERED).max(DRAWER_MIN.min(total)))
        }
    }

    /// The prose's widest line; `None` across the whole window.
    pub fn measure(&self) -> Option<f32> {
        (!self.full).then_some(MEASURE)
    }

    /// The pointer at `x` in a window `total` wide while the drawer's edge is dragged: the drawer
    /// follows it, within what its current place allows (a drag never flips beside and over).
    pub fn drag_to(&mut self, x: f32, shown: bool, total: f32) {
        let width = total - x - HANDLE / 2.0;
        let most = match self.drawer(shown, total) {
            Drawer::Beside(_) => total - HANDLE - MAIN_MIN,
            Drawer::Over(_) => total - UNCOVERED,
            Drawer::Hidden => return,
        };
        self.drawer = Some(width.clamp(DRAWER_MIN, most.max(DRAWER_MIN)));
    }

    /// While dragging: the pointer's moves and its release, wherever they happen in the window.
    pub fn subscription(&self) -> Subscription<Message> {
        if !self.dragging {
            return Subscription::none();
        }
        iced::event::listen_with(|event, _status, _window| match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                Some(Message::Layout(Action::DragTo(position.x)))
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Mouse(mouse::Event::CursorLeft) => Some(Message::Layout(Action::DragEnd)),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drawer_sits_beside_when_both_fit_and_over_when_not() {
        let mut layout = Layout::default();
        assert_eq!(layout.drawer(false, 1200.0), Drawer::Hidden);
        assert_eq!(layout.drawer(true, 1200.0), Drawer::Beside(DRAWER_DEFAULT));
        // a narrow tile: the same drawer, over the conversation
        assert_eq!(layout.drawer(true, 500.0), Drawer::Over(DRAWER_DEFAULT));
        assert_eq!(layout.drawer(true, 200.0), Drawer::Over(DRAWER_MIN));
        // no share of the window: a wider window gives the conversation the room
        assert_eq!(layout.drawer(true, 2400.0), Drawer::Beside(DRAWER_DEFAULT));
        layout.full = true;
        assert_eq!(layout.drawer(true, 1200.0), Drawer::Hidden);
        assert_eq!(layout.measure(), None);
    }

    #[test]
    fn dragging_keeps_the_drawer_in_its_place() {
        let mut layout = Layout::default();
        layout.drag_to(600.0, true, 1000.0);
        assert_eq!(layout.drawer, Some(1000.0 - 600.0 - HANDLE / 2.0));
        // dragged far left: the conversation keeps its minimum
        layout.drag_to(0.0, true, 1000.0);
        assert_eq!(layout.drawer, Some(1000.0 - HANDLE - MAIN_MIN));
        assert!(matches!(layout.drawer(true, 1000.0), Drawer::Beside(_)));
        layout.drag_to(990.0, true, 1000.0);
        assert_eq!(layout.drawer, Some(DRAWER_MIN));
    }
}
