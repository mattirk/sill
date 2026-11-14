//! A spinner of sill's own: a short arc in the widget's CSS colour going round once a second on a
//! faint track, stroked as vector paths and driven by the frame clock. GtkSpinner (a paintable since GTK
//! 4.20, animated SVG in 4.24) follows `gtk-enable-animations` and the window's animation clock,
//! and on this shelf — hidden and shown, placed by hand — it stayed still; the person must see
//! that the agent is working. The tick callback exists only while spinning, so an idle shelf
//! draws nothing.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

const SIZE: i32 = 15;
const LINE: f32 = 2.0;
/// Length of the arc, radians; one turn per `TURN_US`.
const ARC: f64 = 1.3 * PI / 2.0;
const TURN_US: i64 = 1_000_000;

// Drawn as vector strokes into the snapshot, not with cairo: a cairo draw is rasterised into a
// texture each frame, and placed at a fractional offset its edge rows were sampled into the top of
// the circle as a stray bright line.
mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Area {
        /// Frame time of the current frame, µs; the arc's angle follows it (no drift, no state).
        pub now: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Area {
        const NAME: &'static str = "SillSpinner";
        type Type = super::Area;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Area {}

    impl WidgetImpl for Area {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (SIZE, SIZE, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let w = self.obj();
            let (width, height) = (w.width() as f32, w.height() as f32);
            if width.min(height) <= 2.0 * LINE { return; }
            let c = w.color();
            let radius = width.min(height) / 2.0 - LINE;
            let (cx, cy) = (width / 2.0, height / 2.0);
            let stroke = gtk::gsk::Stroke::new(LINE);
            stroke.set_line_cap(gtk::gsk::LineCap::Round);
            let track = gtk::gsk::PathBuilder::new();
            track.add_circle(&gtk::graphene::Point::new(cx, cy), radius);
            snapshot.append_stroke(&track.to_path(), &stroke, &c.with_alpha(0.18));
            // Frame time is system uptime, not time since this animation began: reduce it to
            // one rotation before converting to an angle.
            let start = (self.now.get().rem_euclid(TURN_US) as f64 / TURN_US as f64) * 2.0 * PI;
            let at = |a: f64| (cx + radius * a.cos() as f32, cy + radius * a.sin() as f32);
            let (x0, y0) = at(start);
            let (x1, y1) = at(start + ARC);
            let arc = gtk::gsk::PathBuilder::new();
            arc.move_to(x0, y0);
            arc.svg_arc_to(radius, radius, 0.0, false, true, x1, y1);
            snapshot.append_stroke(&arc.to_path(), &stroke, &c.with_alpha(0.95));
        }
    }
}

glib::wrapper! {
    pub struct Area(ObjectSubclass<imp::Area>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

pub struct Spinner {
    pub area: Area,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl Spinner {
    pub fn new() -> Rc<Self> {
        let area: Area = glib::Object::new();
        area.add_css_class("sill-spinner");
        area.set_valign(gtk::Align::Center);
        area.set_halign(gtk::Align::Center);
        Rc::new(Self { area, tick: RefCell::new(None) })
    }

    /// Spin (a frame-clock tick redraws every frame) or stand still (no tick, no drawing).
    pub fn set_spinning(&self, on: bool) {
        let mut tick = self.tick.borrow_mut();
        match (on, tick.is_some()) {
            (true, false) => {
                *tick = Some(self.area.add_tick_callback(move |w, clock| {
                    w.imp().now.set(clock.frame_time());
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
