//! A spinner of sill's own: a short arc in the widget's CSS colour going round once a second on a
//! faint track, drawn with cairo and driven by the frame clock. GtkSpinner (a paintable since GTK
//! 4.20, animated SVG in 4.24) follows `gtk-enable-animations` and the window's animation clock,
//! and on this shelf — hidden and shown, placed by hand — it stayed still; the person must see
//! that the agent is working. The tick callback exists only while spinning, so an idle shelf
//! draws nothing.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;

pub struct Spinner {
    pub area: gtk::DrawingArea,
    /// Frame time of the current frame, µs; the arc's angle follows it (no drift, no state).
    now: Rc<Cell<i64>>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

const SIZE: i32 = 15;
const LINE: f64 = 2.0;
/// Length of the arc, radians; one turn per `TURN_US`.
const ARC: f64 = 1.3 * PI / 2.0;
const TURN_US: i64 = 1_000_000;

impl Spinner {
    pub fn new() -> Rc<Self> {
        let area = gtk::DrawingArea::new();
        area.add_css_class("sill-spinner");
        area.set_content_width(SIZE);
        area.set_content_height(SIZE);
        area.set_valign(gtk::Align::Center);
        area.set_halign(gtk::Align::Center);
        let now = Rc::new(Cell::new(0i64));
        {
            let now = now.clone();
            area.set_draw_func(move |w, cr, width, height| {
                if width.min(height) <= 2 * LINE as i32 { return; }
                cr.rectangle(0.0, 0.0, width as f64, height as f64);
                cr.clip();
                let c = w.color();
                let (r, g, b) = (c.red() as f64, c.green() as f64, c.blue() as f64);
                let radius = (width.min(height) as f64) / 2.0 - LINE;
                let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
                cr.set_line_width(LINE);
                cr.set_line_cap(gtk::cairo::LineCap::Round);
                cr.set_source_rgba(r, g, b, 0.18);
                cr.arc(cx, cy, radius, 0.0, 2.0 * PI);
                let _ = cr.stroke();
                // Frame time is system uptime, not time since this animation began. Reduce it
                // before converting to an angle so Cairo always receives a single rotation.
                let start = (now.get().rem_euclid(TURN_US) as f64 / TURN_US as f64) * 2.0 * PI;
                cr.set_source_rgba(r, g, b, 0.95);
                cr.arc(cx, cy, radius, start, start + ARC);
                let _ = cr.stroke();
            });
        }
        Rc::new(Self { area, now, tick: RefCell::new(None) })
    }

    /// Spin (a frame-clock tick redraws every frame) or stand still (no tick, no drawing).
    pub fn set_spinning(&self, on: bool) {
        let mut tick = self.tick.borrow_mut();
        match (on, tick.is_some()) {
            (true, false) => {
                let now = self.now.clone();
                *tick = Some(self.area.add_tick_callback(move |w, clock| {
                    now.set(clock.frame_time());
                    w.queue_draw();
                    gtk::glib::ControlFlow::Continue
                }));
            }
            (false, true) => {
                if let Some(id) = tick.take() {
                    id.remove();
                }
            }
            _ => {}
        }
    }
}
