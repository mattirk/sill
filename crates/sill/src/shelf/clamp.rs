//! A clamp of sill's own (the one thing libadwaita was still used for, 2026-11-01): one child, given
//! the whole width up to a maximum and centred beyond it — the conversation's reading measure.

use std::cell::{Cell, RefCell};

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Clamp {
        pub child: RefCell<Option<gtk::Widget>>,
        pub maximum: Cell<i32>,
        pub expanded: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Clamp {
        const NAME: &'static str = "SillClamp";
        type Type = super::Clamp;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Clamp {
        fn dispose(&self) {
            if let Some(c) = self.child.borrow_mut().take() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for Clamp {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.child.borrow().clone() else { return (0, 0, -1, -1) };
            let max = if self.expanded.get() { i32::MAX } else { self.maximum.get() };
            match orientation {
                // the child's natural width, but never asking for more than the maximum
                gtk::Orientation::Horizontal => {
                    let (min, nat, _, _) = child.measure(orientation, for_size);
                    (min, nat.min(max).max(min), -1, -1)
                }
                // the height at the width the child will actually get
                _ => {
                    let width = if for_size < 0 { -1 } else { for_size.min(max) };
                    let (min, nat, _, _) = child.measure(orientation, width);
                    (min, nat, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.borrow().clone() else { return };
            let w = if self.expanded.get() { width } else { width.min(self.maximum.get()) };
            let x = (width - w) / 2;
            child.size_allocate(&gtk::Allocation::new(x, 0, w, height), baseline);
        }
    }
}

glib::wrapper! {
    pub struct Clamp(ObjectSubclass<imp::Clamp>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Clamp {
    pub fn new(child: &impl IsA<gtk::Widget>, maximum: i32) -> Self {
        let c: Self = glib::Object::new();
        child.set_parent(&c);
        *c.imp().child.borrow_mut() = Some(child.clone().upcast());
        c.imp().maximum.set(maximum);
        c
    }

    /// Choose between the reading measure and all available width.
    pub fn set_expanded(&self, expanded: bool) {
        self.imp().expanded.set(expanded);
        self.queue_resize();
    }

    /// The width the child is held to; wider windows centre it.
    pub fn set_maximum_size(&self, maximum: i32) {
        self.imp().maximum.set(maximum);
        self.queue_resize();
    }
}
