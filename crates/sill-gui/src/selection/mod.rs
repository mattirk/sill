//! Selecting text across separately rendered widgets — Markdown paragraphs, code lines, list
//! items, table cells and whole messages — as one selection (docs/sill-gui.md, direction 5).
//!
//! An [`Area`] around a scrollable owns the selection; the [`text`] and [`rich_text`] leaves
//! inside it only lay out, report and draw their text. A press starts a selection (a double
//! press selects a word, a triple one a line, Shift extends), dragging moves its end across
//! leaves and past the edges (the scrollable follows), Ctrl+C copies it in reading order, Ctrl+A
//! takes everything once something is selected, Escape or a press elsewhere clears it. Text that
//! streams in keeps the selection; text that is replaced under it clears it.
//!
//! Leaves outside an area are plain text: they draw, but do not select.
mod area;
mod model;
mod selectable;

pub use area::area;
#[cfg(test)]
pub use model::Selectable;
pub use selectable::{rich_text, text};
