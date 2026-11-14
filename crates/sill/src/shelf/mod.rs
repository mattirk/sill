//! The shelf: a GTK4 window on the left edge, toggled by `sill toggle` (SPEC §3.1, §4.6).
//! One instance per desktop session (GApplication); `toggle` reaches it over D-Bus activation.

mod changes;
mod conversation;
mod files;
mod markdown;
mod session;
mod session_view;
mod settings;
mod tabs;
mod theme;
mod window;
mod x11;

use std::path::PathBuf;

use anyhow::{Result, bail};
use gtk4 as gtk;
use gtk::gio;
use gtk::prelude::*;

pub const APP_ID: &str = "io.github.mattirk.sill";

pub fn run(cwd: Option<PathBuf>, hidden: bool) -> Result<()> {
    let cwd = match cwd {
        Some(c) => std::path::absolute(c)?,
        None => std::env::current_dir()?,
    };
    let app = libadwaita::Application::builder().application_id(APP_ID).build();
    let cwd_for_activate = cwd.clone();
    app.connect_activate(move |app| {
        if let Some(w) = app.windows().first() {
            // a second `sill shelf`: just show the existing one
            window::show(&w);
            return;
        }
        let w = window::build(app, cwd_for_activate.clone());
        if hidden {
            // keep the session warm, show on the first toggle
            w.set_visible(false);
        } else {
            w.present();
        }
    });
    let toggle = gio::SimpleAction::new("toggle", None);
    toggle.connect_activate({
        let app = app.clone();
        move |_, _| {
            if let Some(w) = app.windows().first() {
                window::toggle(w);
            }
        }
    });
    app.add_action(&toggle);
    // `sill theme NAME`: switch the running shelf's theme (also what the Settings dropdown does)
    let set_theme = gio::SimpleAction::new("set-theme", Some(gtk::glib::VariantTy::STRING));
    set_theme.connect_activate(|_, param| {
        if let Some(id) = param.and_then(|p| p.str()) {
            let t = theme::by_id(id);
            theme::apply(&t);
            let mut c = sill_core::config::load();
            c.theme = t.id.to_string();
            let _ = sill_core::config::save(&c);
        }
    });
    app.add_action(&set_theme);
    // no command-line parsing by GTK: we were started by clap
    let code = app.run_with_args::<&str>(&[]);
    if code != glib_exit_ok() {
        bail!("shelf exited with {code:?}");
    }
    Ok(())
}

fn glib_exit_ok() -> gtk::glib::ExitCode {
    gtk::glib::ExitCode::SUCCESS
}

/// `sill theme NAME`: ask the running shelf to switch theme (saved for next start too).
pub fn set_theme(name: &str) -> Result<()> {
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        // no shelf running: just save it
        let t = theme::by_id(name);
        let mut c = sill_core::config::load();
        c.theme = t.id.to_string();
        return sill_core::config::save(&c);
    }
    app.activate_action("set-theme", Some(&name.to_variant()));
    Ok(())
}

/// `sill toggle`: ask the running shelf to show/hide itself.
pub fn toggle() -> Result<()> {
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        bail!("the shelf is not running (start it with `sill shelf`)");
    }
    app.activate_action("toggle", None);
    Ok(())
}
