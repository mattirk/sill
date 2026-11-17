//! How the window is divided, in three modes (docs/sill-gui.md, *Window and layout modes*):
//!
//! - **Shelf**: the minimal conversation on the main display's left edge, a share of its width
//!   (35 % by default, 15–40 %), the drawer a quick look over it (Ctrl+Shift+F);
//! - **Drawer**: the conversation with the drawer always beside it, at the working size;
//! - **Full**: the conversation alone across the working size, without the reading measure.
//!
//! Ctrl+Shift+D goes from Shelf to Drawer and back to Shelf from Drawer or Full; Ctrl+Shift+F goes
//! into Full and back to the mode it came from. Only Shelf has a size rule of its own; the working size is whatever the
//! window manager or the person made it (where the window goes: `geometry.rs`). When a window
//! cannot fit the drawer beside the conversation (a narrow tile), the drawer opens over it.
use iced::{Event, Subscription, mouse};

use crate::Message;

/// The drawer's width until the person drags it.
/// 30 % narrower than the first 280 (the person, 2026-11-05).
pub const DRAWER_DEFAULT: f32 = 196.0;
pub const DRAWER_MIN: f32 = 160.0;
/// What the conversation keeps at least with the drawer beside it.
pub const MAIN_MIN: f32 = 360.0;
/// What stays visible of the conversation beside a drawer over it: a press there closes it.
pub const UNCOVERED: f32 = 48.0;
/// The conversation's reading measure.
pub const MEASURE: f32 = 800.0;
/// The grip between the conversation and the drawer.
pub const HANDLE: f32 = 7.0;
/// Shelf mode's share of the main display's usable width, until set otherwise, and the least
/// and the most it may be (the person, 2026-11-05; the bounds of dragging its edge, 2026-11-07).
pub const SHELF_SHARE: f32 = 0.35;
pub const SHELF_MIN: f32 = 0.15;
pub const SHELF_MAX: f32 = 0.4;
/// The working size's share of the display's usable width before the person has made one.
pub const WORK_SHARE: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The minimal conversation, at most the shelf share of the monitor.
    #[default]
    Shelf,
    /// The conversation with the drawer beside it.
    Drawer,
    /// The conversation alone, across the window.
    Full,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Shelf => "shelf",
            Mode::Drawer => "drawer",
            Mode::Full => "full",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "shelf" => Some(Mode::Shelf),
            "drawer" => Some(Mode::Drawer),
            "full" => Some(Mode::Full),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Layout {
    pub mode: Mode,
    /// The window is the system's own (the Mac): no modes and no placement of sill's; the drawer
    /// is beside the conversation where both fit, whichever mode is named.
    pub plain: bool,
    /// The mode Full was entered from, where Ctrl+Shift+F goes back to.
    pub before_full: Mode,
    /// The width the person dragged the drawer to; for this run of the window.
    pub drawer: Option<f32>,
    /// The drawer's edge is being dragged.
    pub dragging: bool,
    /// The drawer's width when the drag began: what the conversation's rows stay laid out
    /// against until the release, so a long conversation is not measured anew at every pointer
    /// move (`App::feed`).
    pub drag_from: Option<f32>,
}

impl Default for Layout {
    fn default() -> Self {
        Self { mode: Mode::Shelf, plain: false, before_full: Mode::Drawer, drawer: None, dragging: false, drag_from: None }
    }
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
    Mode(Mode),
    /// Ctrl+Shift+D: Shelf → Drawer; Drawer or Full → Shelf.
    ToggleShelf,
    /// Ctrl+Shift+F: into Full, and back to the mode it was entered from.
    ToggleFull,
    DragStart,
    /// The pointer's x in the window while dragging.
    DragTo(f32),
    DragEnd,
}

impl Layout {
    /// The drawer's width in pixels, wherever it is.
    pub fn drawer_width(&self) -> f32 {
        self.wanted()
    }

    fn wanted(&self) -> f32 {
        self.drawer.unwrap_or(DRAWER_DEFAULT).max(DRAWER_MIN)
    }

    /// The drawer's place, `shown` (always in Drawer mode, a quick look in the others): beside
    /// the conversation in Drawer mode when both fit, else over it.
    pub fn drawer(&self, shown: bool, total: f32) -> Drawer {
        if !shown {
            return Drawer::Hidden;
        }
        let wanted = self.wanted();
        if (self.plain || self.mode == Mode::Drawer) && total - wanted - HANDLE >= MAIN_MIN {
            Drawer::Beside(wanted)
        } else {
            Drawer::Over(wanted.min(total - UNCOVERED).max(DRAWER_MIN.min(total)))
        }
    }

    /// The prose's widest line; `None` across the whole window.
    pub fn measure(&self) -> Option<f32> {
        (self.mode != Mode::Full).then_some(MEASURE)
    }

    /// The width the conversation's rows are laid out at in a window `total` wide, with the
    /// drawer `shown`: what their measured heights hold for. Beside the drawer, the room left;
    /// within the reading measure where there is one. While the drawer's edge is dragged, the
    /// width from before the drag: the rows in view are built and reflow anyway, the rest keep
    /// their heights until the release, when every row is measured once at the new width.
    pub fn row_width(&self, shown: bool, total: f32) -> f32 {
        let drawer = match self.drawer(shown, total) {
            Drawer::Beside(_) => Some(self.drag_from.unwrap_or_else(|| self.wanted())),
            _ => None,
        };
        let room = match drawer {
            Some(width) => total - width - HANDLE,
            None => total,
        };
        match self.measure() {
            Some(measure) => room.min(measure),
            None => room,
        }
    }

    /// Where Ctrl+Shift+D and Ctrl+Shift+E lead from the current mode.
    pub fn after(&self, action: Action) -> Mode {
        match (action, self.mode) {
            (Action::Mode(mode), _) => mode,
            (Action::ToggleShelf, Mode::Shelf) => Mode::Drawer,
            (Action::ToggleShelf, _) => Mode::Shelf,
            (Action::ToggleFull, Mode::Full) => self.before_full,
            (Action::ToggleFull, _) => Mode::Full,
            _ => self.mode,
        }
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
            Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::Layout(Action::DragTo(position.x))),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) | Event::Mouse(mouse::Event::CursorLeft) => Some(Message::Layout(Action::DragEnd)),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_window_has_the_drawer_beside_when_it_fits_whatever_the_mode() {
        let mut layout = Layout { plain: true, ..Layout::default() };
        assert_eq!(layout.drawer(false, 1200.0), Drawer::Hidden);
        assert_eq!(layout.drawer(true, 1200.0), Drawer::Beside(DRAWER_DEFAULT));
        assert!(matches!(layout.drawer(true, 400.0), Drawer::Over(_)));
        layout.plain = false;
        assert!(matches!(layout.drawer(true, 1200.0), Drawer::Over(_)));
    }

    #[test]
    fn the_rows_width_follows_the_drawer_only_where_it_narrows_them_and_not_while_dragged() {
        let mut layout = Layout { mode: Mode::Drawer, ..Layout::default() };
        // wider than the measure: the drawer takes nothing from the rows
        assert_eq!(layout.row_width(true, 1400.0), MEASURE);
        layout.drawer = Some(300.0);
        assert_eq!(layout.row_width(true, 1400.0), MEASURE);
        // narrower: the room beside the drawer
        assert_eq!(layout.row_width(true, 900.0), 900.0 - 300.0 - HANDLE);
        assert_eq!(layout.row_width(false, 900.0), 800.0);
        // dragged: the width from before the drag until the release
        layout.drag_from = Some(300.0);
        layout.drag_to(400.0, true, 900.0);
        assert_ne!(layout.drawer, Some(300.0));
        assert_eq!(layout.row_width(true, 900.0), 900.0 - 300.0 - HANDLE);
        layout.drag_from = None;
        assert_eq!(layout.row_width(true, 900.0), 900.0 - layout.drawer_width() - HANDLE);
        // Full mode: no measure, the whole window
        layout.mode = Mode::Full;
        assert_eq!(layout.row_width(true, 1400.0), 1400.0);
    }

    #[test]
    fn the_drawer_is_beside_in_drawer_mode_and_a_look_over_the_conversation_elsewhere() {
        let mut layout = Layout { mode: Mode::Drawer, ..Layout::default() };
        assert_eq!(layout.drawer(false, 1200.0), Drawer::Hidden);
        assert_eq!(layout.drawer(true, 1200.0), Drawer::Beside(DRAWER_DEFAULT));
        // a narrow tile: the same drawer, over the conversation
        assert_eq!(layout.drawer(true, 500.0), Drawer::Over(DRAWER_DEFAULT));
        assert_eq!(layout.drawer(true, 200.0), Drawer::Over(DRAWER_MIN));
        layout.mode = Mode::Shelf;
        assert_eq!(layout.drawer(true, 1200.0), Drawer::Over(DRAWER_DEFAULT));
        assert_eq!(layout.measure(), Some(MEASURE));
        layout.mode = Mode::Full;
        assert_eq!(layout.measure(), None);
    }

    #[test]
    fn the_keys_go_between_the_modes() {
        let mut layout = Layout::default();
        assert_eq!(layout.after(Action::ToggleShelf), Mode::Drawer);
        layout.mode = Mode::Drawer;
        assert_eq!(layout.after(Action::ToggleFull), Mode::Full);
        assert_eq!(layout.after(Action::ToggleShelf), Mode::Shelf);
        layout.mode = Mode::Full;
        layout.before_full = Mode::Shelf;
        assert_eq!(layout.after(Action::ToggleFull), Mode::Shelf);
        assert_eq!(layout.after(Action::ToggleShelf), Mode::Shelf);
    }

    #[test]
    fn dragging_keeps_the_drawer_in_its_place() {
        let mut layout = Layout { mode: Mode::Drawer, ..Layout::default() };
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
