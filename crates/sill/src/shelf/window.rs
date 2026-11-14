//! The shelf window: session bar, conversation, prompt (SPEC §3.1). Placement and visibility (§4.6).

use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use adw::prelude::*;
use sill_core::acp::Command;

use super::conversation::{CSS, Conversation};
use super::{session, x11};

const MAIN_SESSION: &str = "main";
const AGENT: &str = "claude-agent-acp";

pub fn build(app: &adw::Application, cwd: PathBuf) -> gtk::Window {
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("sill")
        .decorated(false)
        .default_width(700)
        .default_height(1000)
        .build();

    // --- session bar
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    bar.set_margin_top(8);
    bar.set_margin_bottom(4);
    let title = gtk::Label::new(Some(&format!("sill · {}", cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    let status = gtk::Label::new(Some("starting…"));
    status.add_css_class("dim-label");
    bar.append(&title);
    bar.append(&status);

    // --- conversation
    let conv = Conversation::new(status.clone());

    // --- prompt
    let prompt = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(10)
        .right_margin(10)
        .top_margin(8)
        .bottom_margin(8)
        .accepts_tab(false)
        .build();
    prompt.add_css_class("sill-prompt");
    let prompt_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .max_content_height(220)
        .propagate_natural_height(true)
        .child(&prompt)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    prompt_scroll.add_css_class("frame");

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&bar);
    column.append(&conv.root);
    column.append(&prompt_scroll);
    window.set_content(Some(&column));

    // --- session
    let driver: Rc<std::cell::RefCell<Option<session::Driver>>> = Rc::new(std::cell::RefCell::new(None));
    {
        let conv = conv.clone();
        let driver = driver.clone();
        let status = status.clone();
        match session::start(MAIN_SESSION, &cwd, AGENT) {
            Ok(d) => {
                let events = d.events.clone();
                *driver.borrow_mut() = Some(d);
                glib::spawn_future_local(async move {
                    while let Ok(ev) = events.recv().await {
                        conv.handle(ev);
                    }
                });
            }
            Err(e) => {
                status.set_text("failed to start");
                conv.add_note(&format!("could not start the session: {e:#}"));
            }
        }
    }

    // Enter sends, Shift+Enter inserts a newline
    let keys = gtk::EventControllerKey::new();
    {
        let prompt = prompt.clone();
        let conv = conv.clone();
        let driver = driver.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                let buf = prompt.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                let text = text.trim().to_string();
                if text.is_empty() {
                    return glib::Propagation::Stop;
                }
                if let Some(d) = driver.borrow().as_ref() {
                    match d.commands.try_send(Command::Prompt(text.clone())) {
                        Ok(()) => {
                            conv.add_user(&text);
                            buf.set_text("");
                        }
                        Err(_) => conv.add_note("the agent is busy; wait for the turn to end"),
                    }
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    prompt.add_controller(keys);

    // Escape hides
    let esc = gtk::EventControllerKey::new();
    {
        let w = window.clone();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                w.set_visible(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(esc);

    // place on the left edge each time it is mapped
    {
        let prompt = prompt.clone();
        window.connect_map(move |w| {
            let w: gtk::Window = w.clone().upcast();
            let p = prompt.clone();
            glib::idle_add_local_once(move || {
                if let Some(area) = x11::work_area(&w) {
                    x11::place(&w, &area);
                }
                x11::activate(&w);
                p.grab_focus();
            });
        });
    }
    window.upcast()
}

pub fn show(w: &gtk::Window) {
    w.present();
    x11::activate(w);
}

pub fn toggle(w: &gtk::Window) {
    if w.is_visible() && w.is_active() {
        w.set_visible(false);
    } else {
        show(w);
    }
}
