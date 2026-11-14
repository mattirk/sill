//! The herring in the top-left corner: sill's mark (`herring-silhouette.svg`: one solid fish, the
//! tail part of the body with a notch, the eye a hole), drawn with cairo in the widget's CSS colour
//! like [`super::spinner`]. Still by default; `flick` swims one stroke — a travelling wave down the
//! body, the nose pinned — and stops. The tick callback exists only during a flick, so the mark
//! costs nothing at rest.

use std::cell::Cell;
use std::f64::consts::PI;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;

pub struct Herring {
    pub area: gtk::DrawingArea,
    /// Frame time the current flick started, µs; `None` while still.
    started: Rc<Cell<Option<i64>>>,
    /// A tick callback is live (it removes itself when the flick ends).
    ticking: Rc<Cell<bool>>,
}

const SIZE: i32 = 18;
/// One stroke of the tail.
const FLICK_US: f64 = 700_000.0;
/// The glyph's 24-unit grid: the nose and the tail's tips.
const NOSE: f64 = 1.5;
const TAIL: f64 = 22.5;

/// Vertical displacement of the body at `x` for `phase` in 0..1: nothing at the nose, 0.9 units
/// at the tail, the tail lagging the body by a third of a turn so the fish bends, not wobbles.
fn swim(x: f64, phase: f64) -> f64 {
    let u = (x - NOSE) / (TAIL - NOSE);
    0.9 * u * u * (2.0 * PI * phase - 0.66 * PI * u).sin()
}

impl Herring {
    pub fn new() -> Rc<Self> {
        let area = gtk::DrawingArea::new();
        area.add_css_class("sill-herring");
        area.set_content_width(SIZE);
        area.set_content_height(SIZE);
        area.set_valign(gtk::Align::Center);
        area.set_halign(gtk::Align::Center);
        let started = Rc::new(Cell::new(None));
        {
            let started = started.clone();
            area.set_draw_func(move |w, cr, width, height| {
                let c = w.color();
                let phase = started.get().map(|t0| (w.frame_clock().map(|c| c.frame_time()).unwrap_or(t0) - t0) as f64 / FLICK_US);
                let phase = phase.unwrap_or(0.0).clamp(0.0, 1.0);
                let s = width.min(height) as f64 / 24.0;
                cr.translate((width as f64 - 24.0 * s) / 2.0, (height as f64 - 24.0 * s) / 2.0);
                cr.scale(s, s);
                cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
                let y = |x: f64, y: f64| y + swim(x, phase);
                // the body: over the back to the tail, the notched tail, back under the belly
                cr.move_to(NOSE, 12.0);
                cr.curve_to(4.5, y(4.5, 6.5), 12.0, y(12.0, 5.0), 17.5, y(17.5, 10.5));
                cr.line_to(TAIL, y(TAIL, 6.5));
                cr.line_to(20.5, y(20.5, 12.0));
                cr.line_to(TAIL, y(TAIL, 17.5));
                cr.line_to(17.5, y(17.5, 13.5));
                cr.curve_to(12.0, y(12.0, 19.0), 4.5, y(4.5, 17.5), NOSE, 12.0);
                cr.close_path();
                // the eye: wound the other way, a hole under cairo's winding rule
                cr.new_sub_path();
                cr.arc_negative(6.0, 11.0, 1.3, 0.0, -2.0 * PI);
                let _ = cr.fill();
            });
        }
        Rc::new(Self { area, started, ticking: Rc::new(Cell::new(false)) })
    }

    /// One stroke of the tail, then still again. A flick during a flick restarts it.
    pub fn flick(&self) {
        let Some(clock) = self.area.frame_clock() else { return };
        self.started.set(Some(clock.frame_time()));
        if self.ticking.replace(true) {
            return;
        }
        let started = self.started.clone();
        let ticking = self.ticking.clone();
        self.area.add_tick_callback(move |w, clock| {
            w.queue_draw();
            let done = started.get().map(|t0| (clock.frame_time() - t0) as f64 >= FLICK_US).unwrap_or(true);
            if done {
                started.set(None);
                ticking.set(false);
                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });
    }
}
