//! The herring in the top-left corner: sill's mark, one
//! solid fish with a notched tail and the eye a hole. Still at rest; when the view goes between
//! the session and the backend it swims one stroke — a travelling wave down the body, the nose
//! pinned — and stops. It asks for frames only during the stroke.
use std::f32::consts::PI;
use std::time::{Duration, Instant};

use iced::widget::canvas;
use iced::widget::canvas::{Fill, Frame, Geometry, Path, fill};
use iced::{Color, Element, Point, Rectangle, Renderer, Theme, mouse};

use crate::Message;

/// One stroke of the tail.
const FLICK: Duration = Duration::from_millis(700);
/// The glyph's 24-unit grid: the nose and the tail's tips.
const NOSE: f32 = 1.5;
const TAIL: f32 = 22.5;

/// Vertical displacement of the body at `x` for `phase` in 0..1: nothing at the nose, 0.9 units
/// at the tail, the tail lagging the body by a third of a turn so the fish bends, not wobbles.
fn swim(x: f32, phase: f32) -> f32 {
    let u = (x - NOSE) / (TAIL - NOSE);
    0.9 * u * u * (2.0 * PI * phase - 0.66 * PI * u).sin()
}

/// The mark, `size` square in `color`; swimming since `flicked`, if that is under a stroke ago.
pub fn herring<'a>(size: f32, color: Color, flicked: Option<Instant>) -> Element<'a, Message> {
    canvas(Herring { color, flicked }).width(size).height(size).into()
}

struct Herring {
    color: Color,
    flicked: Option<Instant>,
}

impl Herring {
    /// Where the stroke is, 0..1; `None` at rest.
    fn phase(&self) -> Option<f32> {
        let elapsed = self.flicked?.elapsed();
        (elapsed < FLICK).then(|| elapsed.as_secs_f32() / FLICK.as_secs_f32())
    }
}

impl canvas::Program<Message> for Herring {
    type State = ();

    fn update(&self, _state: &mut (), event: &iced::Event, _bounds: Rectangle, _cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        // during the stroke only: still, it asks for nothing
        (self.phase().is_some() && matches!(event, iced::Event::Window(iced::window::Event::RedrawRequested(_)))).then(canvas::Action::request_redraw)
    }

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let phase = self.phase().unwrap_or(0.0);
        let s = bounds.width.min(bounds.height) / 24.0;
        let (dx, dy) = ((bounds.width - 24.0 * s) / 2.0, (bounds.height - 24.0 * s) / 2.0);
        let at = |x: f32, y: f32| Point::new(dx + x * s, dy + (y + swim(x, phase)) * s);
        let fish = Path::new(|p| {
            // the body: over the back to the tail, the notched tail, back under the belly
            p.move_to(at(NOSE, 12.0));
            p.bezier_curve_to(at(4.5, 6.5), at(12.0, 5.0), at(17.5, 10.5));
            p.line_to(at(TAIL, 6.5));
            p.line_to(at(20.5, 12.0));
            p.line_to(at(TAIL, 17.5));
            p.line_to(at(17.5, 13.5));
            p.bezier_curve_to(at(12.0, 19.0), at(4.5, 17.5), at(NOSE, 12.0));
            p.close();
            // the eye: a hole, under the even-odd rule (the nose is pinned, so it stays put)
            p.circle(Point::new(dx + 6.0 * s, dy + 11.0 * s), 1.3 * s);
        });
        frame.fill(&fish, Fill { style: fill::Style::Solid(self.color), rule: fill::Rule::EvenOdd });
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nose_stays_and_the_tail_swims_one_stroke() {
        assert_eq!(swim(NOSE, 0.3), 0.0, "the nose is pinned");
        assert!(swim(TAIL, 0.0).abs() < 0.9 && swim(TAIL, 0.0).abs() > 0.5);
        // a whole stroke ends where it began
        assert!((swim(TAIL, 1.0) - swim(TAIL, 0.0)).abs() < 1e-4);
        let still = Herring { color: Color::WHITE, flicked: None };
        assert_eq!(still.phase(), None);
        let done = Herring { color: Color::WHITE, flicked: Instant::now().checked_sub(FLICK * 2) };
        assert_eq!(done.phase(), None, "after one stroke it is still again");
    }
}
