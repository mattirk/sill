//! A long conversation lays out only what is near the view (docs/sill-gui.md, *Long
//! conversations*). iced lays out the whole widget tree for every message — every scroll step,
//! every streamed chunk — so 2000 rows cost ~30 ms a frame. Here each row's height is measured
//! once, by the layout that draws it, and kept with what decides it (the row's content and the
//! width it was laid out at); a row far from the view is a gap of its known height, so the
//! scroll position and the bar stay exact. A row not measured yet is always built: it measures
//! itself in that frame.
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector};

use crate::Message;

/// Row heights by row key: the signature they were measured for, and the height.
pub type Heights = RefCell<HashMap<String, (u64, f32)>>;

/// What the plan builds: a row, or a run of rows left out as one gap of their height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slot {
    Row(usize),
    Gap { first: usize, height: f32 },
}

/// Emitted around a conversation row during `Widget::operate` so [`crate::selection::Area`] knows
/// which row a [`crate::selection::Selectable`] leaf belongs to.
pub struct RowScope(pub Option<usize>);

/// The gap between the conversation's rows.
pub const SPACING: f32 = 12.0;

/// Which rows to build for a view `viewport` high scrolled to `offset` (to the end while
/// following the tail): those within a screen of the view, and any not measured yet; the rest
/// become gaps. `spacing` is the column's gap between children, which a gap of k rows keeps:
/// k heights and k − 1 spacings, so the column is as tall as with every row built. With no
/// viewport known yet, every row is built.
#[cfg(test)]
pub fn plan(heights: &[Option<f32>], spacing: f32, offset: f32, viewport: f32, follow_tail: bool) -> Vec<Slot> {
    plan_with_pinned(heights, spacing, offset, viewport, follow_tail, None)
}

/// Like [`plan`], also guaranteeing that `pinned` (the row of the current Find match) is built.
pub fn plan_with_pinned(heights: &[Option<f32>], spacing: f32, offset: f32, viewport: f32, follow_tail: bool, pinned: Option<usize>) -> Vec<Slot> {
    if viewport <= 0.0 {
        return (0..heights.len()).map(Slot::Row).collect();
    }
    let total: f32 = heights.iter().map(|h| h.unwrap_or(0.0) + spacing).sum();
    let top = if follow_tail { (total - viewport).max(0.0) } else { offset };
    let (low, high) = (top - viewport, top + 2.0 * viewport);
    let mut slots = Vec::new();
    let mut at = 0.0;
    for (index, height) in heights.iter().enumerate() {
        let h = height.unwrap_or(0.0);
        let near = at + h >= low && at <= high;
        if height.is_none() || near || pinned == Some(index) {
            slots.push(Slot::Row(index));
        } else {
            match slots.last_mut() {
                Some(Slot::Gap { height, .. }) => *height += spacing + h,
                _ => slots.push(Slot::Gap { first: index, height: h }),
            }
        }
        at += h + spacing;
    }
    slots
}

/// A row's signature: what its height depends on, with the layout it is laid out in.
pub fn signature(parts: impl Hash, layout: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    layout.hash(&mut hasher);
    hasher.finish()
}

/// A key for the keyed column: the row's identity, so a row keeps its widget state (a selection,
/// a scrolled code block) while others come and go around it.
pub fn key(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// A conversation row: tells operations its `index` ([`RowScope`]) and, when it has a `key`,
/// records its laid-out height under it and `signature`. Transparent otherwise: it has no tree
/// node of its own, so wrapping a row changes nothing of its state.
pub struct Measured<'a> {
    content: Element<'a, Message>,
    index: usize,
    measure: Option<(&'a str, u64, &'a Heights)>,
}

pub fn measured<'a>(content: impl Into<Element<'a, Message>>, index: usize, key: &'a str, signature: u64, heights: &'a Heights) -> Measured<'a> {
    Measured { content: content.into(), index, measure: Some((key, signature, heights)) }
}

/// A row after the measured ones (a question placed at the end, a problem): scoped, not measured.
pub fn scoped<'a>(index: usize, content: impl Into<Element<'a, Message>>) -> Measured<'a> {
    Measured { content: content.into(), index, measure: None }
}

impl Widget<Message, Theme, Renderer> for Measured<'_> {
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
        let node = self.content.as_widget_mut().layout(tree, renderer, limits);
        if let Some((key, signature, heights)) = self.measure {
            heights.borrow_mut().insert(key.to_string(), (signature, node.size().height));
        }
        node
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        operation.custom(None, layout.bounds(), &mut RowScope(Some(self.index)));
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
        operation.custom(None, layout.bounds(), &mut RowScope(None));
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

impl<'a> From<Measured<'a>> for Element<'a, Message> {
    fn from(measured: Measured<'a>) -> Self {
        Element::new(measured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_far_from_the_view_become_gaps_of_their_height() {
        // 100 rows of 50 with 10 between: 6000 high; a 300 view scrolled to 3000
        let heights = vec![Some(50.0); 100];
        let slots = plan(&heights, 10.0, 3000.0, 300.0, false);
        let built: Vec<usize> = slots
            .iter()
            .filter_map(|s| match s {
                Slot::Row(i) => Some(*i),
                _ => None,
            })
            .collect();
        // a screen above and two below the top of the view: rows 45..=60
        assert_eq!(built.first(), Some(&45));
        assert_eq!(built.last(), Some(&60));
        // the gaps keep the column's height: 45 rows above are 45 heights and 44 spacings
        assert_eq!(slots[0], Slot::Gap { first: 0, height: 45.0 * 50.0 + 44.0 * 10.0 });
        let column: f32 = slots
            .iter()
            .map(|s| match s {
                Slot::Row(_) => 50.0,
                Slot::Gap { height, .. } => *height,
            })
            .sum::<f32>()
            + (slots.len() - 1) as f32 * 10.0;
        assert_eq!(column, 100.0 * 50.0 + 99.0 * 10.0);
    }

    #[test]
    fn the_tail_unmeasured_rows_and_an_unknown_view_are_built() {
        let mut heights = vec![Some(50.0); 100];
        heights[10] = None;
        let slots = plan(&heights, 10.0, 0.0, 300.0, true);
        assert!(slots.contains(&Slot::Row(10)), "not measured yet: built");
        assert!(slots.contains(&Slot::Row(99)), "following the tail");
        assert!(!slots.contains(&Slot::Row(50)));
        assert_eq!(plan(&heights, 10.0, 0.0, 0.0, false).len(), 100);
    }
}
