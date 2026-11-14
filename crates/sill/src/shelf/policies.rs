//! Policy-library management in Settings. Session launch rules live beside the session.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use sill_proto::admin as api;

struct Ui {
    names: gtk::DropDown,
    choose: gtk::Button,
    refresh: gtk::Button,
    preview: gtk::TextView,
    source: gtk::Label,
    status: gtk::Label,
    summary: gtk::Label,
    file: gtk::Label,
    name: gtk::Entry,
    check: gtk::Button,
    import: gtk::Button,
    replace: gtk::CheckButton,
    text: RefCell<Option<String>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
}

fn work<T: Send + 'static>(operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static, done: impl FnOnce(Result<T, String>) + 'static) {
    glib::spawn_future_local(async move {
        let result = gtk::gio::spawn_blocking(move || operation().map_err(|e| format!("{e:#}"))).await;
        done(result.unwrap_or_else(|_| Err("Policy operation did not complete".into())));
    });
}

impl Ui {
    fn message(&self, text: &str, error: bool) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
        if error { self.status.add_css_class("error"); } else { self.status.remove_css_class("error"); }
    }

    fn controls(&self) {
        let ready = self.text.borrow().is_some() && !self.busy.get();
        self.check.set_sensitive(ready);
        self.import.set_sensitive(ready && !self.name.text().trim().is_empty());
        self.names.set_sensitive(!self.busy.get());
        self.choose.set_sensitive(!self.busy.get());
        self.refresh.set_sensitive(!self.busy.get());
        self.name.set_sensitive(!self.busy.get());
        self.replace.set_sensitive(!self.busy.get());
    }

    fn inspect(self: &Rc<Self>) {
        let Some(item) = self.names.selected_item().and_downcast::<gtk::StringObject>() else { return };
        let name = item.string().to_string();
        self.text.replace(None);
        self.file.set_text("No file selected");
        self.file.set_tooltip_text(None);
        self.replace.set_active(false);
        self.controls();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.source.set_text(&format!("Installed base policy: {name}"));
        self.preview.buffer().set_text("Loading…");
        let ui = self.clone();
        work(move || super::client::call::<api::PolicyEntry>("_sill/policy.get", api::PolicyKey { name }), move |result| {
            if generation != ui.generation.get() { return; }
            match result {
                Ok(policy) => ui.preview.buffer().set_text(&policy.text),
                Err(e) => { ui.preview.buffer().set_text(""); ui.message(&format!("Could not inspect policy: {e}"), true); }
            }
        });
    }

    fn refresh(self: &Rc<Self>, select: Option<String>) {
        self.busy.set(true);
        self.controls();
        let ui = self.clone();
        work(|| {
            let mut names = Vec::new();
            let mut after = None;
            loop {
                let page: api::Names = super::client::call("_sill/policy.list", api::NamesList { after: after.clone(), limit: Some(128) })?;
                names.extend(page.names);
                if names.len() > 4096 { anyhow::bail!("Too many installed policies to display"); }
                let Some(next) = page.next else { break };
                if after.as_ref().is_some_and(|last| next <= *last) { anyhow::bail!("Invalid policy list cursor"); }
                after = Some(next);
            }
            Ok(names)
        }, move |result| {
          ui.busy.set(false);
          ui.controls();
          match result {
            Ok(names) => {
                if ui.status.has_css_class("error") { ui.message("", false); }
                ui.summary.set_text(&format!("Policies · {} installed", names.len()));
                let strings: Vec<&str> = names.iter().map(String::as_str).collect();
                ui.names.set_model(Some(&gtk::StringList::new(&strings)));
                if let Some(name) = select {
                    if let Some(i) = names.iter().position(|n| *n == name) { ui.names.set_selected(i as u32); }
                }
                ui.inspect();
            }
            Err(e) => ui.message(&format!("Could not list policies: {e}"), true),
          }
        });
    }
}

/// The status label deliberately sits outside the expander, so errors survive collapse.
pub fn build() -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let folded = gtk::Expander::new(None);
    let summary = gtk::Label::new(Some("Policies"));
    summary.add_css_class("sill-settings-title");
    folded.set_label_widget(Some(&summary));
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("sill-settings-card");
    card.set_margin_top(6);
    card.set_margin_start(12);
    card.set_margin_end(12);
    card.set_margin_top(12);
    card.set_margin_bottom(12);
    folded.set_child(Some(&card));
    root.append(&folded);

    let installed = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let names = gtk::DropDown::from_strings(&[]);
    names.set_hexpand(true);
    names.set_enable_search(true);
    let refresh = gtk::Button::with_label("Refresh");
    installed.append(&names);
    installed.append(&refresh);
    card.append(&installed);
    let source = label("Select an installed base policy, or choose a file to review.");
    source.add_css_class("dim-label");
    card.append(&source);
    let preview = gtk::TextView::builder().editable(false).cursor_visible(false).monospace(true).wrap_mode(gtk::WrapMode::WordChar).build();
    let scroll = gtk::ScrolledWindow::builder().min_content_height(180).max_content_height(300).hscrollbar_policy(gtk::PolicyType::Never).child(&preview).build();
    scroll.add_css_class("frame");
    card.append(&scroll);

    let explanation = label("Check and import complete TOML policies. Choose a file from this computer; import installs it on the connected Sill host. Repository fragments are inspected from their session.");
    explanation.add_css_class("dim-label");
    explanation.add_css_class("sill-settings-sub");
    card.append(&explanation);
    let files = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let choose = gtk::Button::with_label("Choose file…");
    let file = label("No file selected");
    file.set_hexpand(true);
    file.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    file.set_wrap(false);
    let check = gtk::Button::with_label("Check");
    files.append(&choose);
    files.append(&file);
    files.append(&check);
    card.append(&files);
    let name = gtk::Entry::builder().placeholder_text("Import as policy name").hexpand(true).build();
    name.set_max_length(128);
    card.append(&name);
    let replace = gtk::CheckButton::with_label("Replace an existing user policy");
    card.append(&replace);
    let import = gtk::Button::with_label("Import policy");
    import.set_halign(gtk::Align::Start);
    card.append(&import);
    let status = label("");
    status.set_visible(false);
    status.add_css_class("sill-settings-sub");
    root.append(&status);

    let ui = Rc::new(Ui { names, choose: choose.clone(), refresh: refresh.clone(), preview, source, status, summary, file, name, check, import, replace, text: RefCell::new(None), generation: Cell::new(0), busy: Cell::new(false) });
    ui.controls();
    let weak = Rc::downgrade(&ui);
    ui.names.connect_selected_notify(move |_| { if let Some(ui) = weak.upgrade() { ui.inspect(); } });
    let weak = Rc::downgrade(&ui);
    refresh.connect_clicked(move |_| { if let Some(ui) = weak.upgrade() { ui.refresh(None); } });
    let weak = Rc::downgrade(&ui);
    ui.name.connect_changed(move |_| { if let Some(ui) = weak.upgrade() { ui.controls(); } });
    let weak = Rc::downgrade(&ui);
    ui.replace.connect_toggled(move |b| { if let Some(ui) = weak.upgrade() { ui.import.set_label(if b.is_active() { "Replace policy" } else { "Import policy" }); } });

    let weak = Rc::downgrade(&ui);
    choose.connect_clicked(move |button| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.busy.get() { return; }
        let dialog = gtk::FileDialog::builder().title("Choose a policy to review").modal(true).build();
        let parent = button.root().and_downcast::<gtk::Window>();
        dialog.open(parent.as_ref(), None::<&gtk::gio::Cancellable>, move |result| {
            let file = match result { Ok(f) => f, Err(_) => return };
            let Some(path) = file.path() else { ui.message("Choose a local file on this computer", true); return };
            let display = path.display().to_string();
            let proposed_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            ui.busy.set(true);
            ui.text.replace(None);
            ui.controls();
            ui.message("Reading policy file…", false);
            let ui2 = ui.clone();
            work(move || {
                use std::io::Read;
                let mut text = String::new();
                std::fs::File::open(path)?.take(api::TEXT_BYTES as u64 + 1).read_to_string(&mut text)?;
                if text.len() > api::TEXT_BYTES { anyhow::bail!("Policy file exceeds 1 MiB"); }
                Ok(text)
            }, move |result| {
                ui2.busy.set(false);
                match result {
                    Ok(text) => {
                        ui2.generation.set(ui2.generation.get() + 1);
                        ui2.names.set_selected(gtk::INVALID_LIST_POSITION);
                        ui2.file.set_text(&display);
                        ui2.file.set_tooltip_text(Some(&display));
                        ui2.source.set_text("Selected file — review before importing");
                        ui2.preview.buffer().set_text(&text);
                        ui2.text.replace(Some(text));
                        ui2.name.set_text(&proposed_name);
                        ui2.replace.set_active(false);
                        ui2.message("File loaded. Check the policy and choose its installed name before importing.", false);
                    }
                    Err(e) => ui2.message(&format!("Could not read policy file: {e}"), true),
                }
                ui2.controls();
            });
        });
    });

    let weak = Rc::downgrade(&ui);
    ui.check.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(text) = ui.text.borrow().clone() else { return };
        ui.busy.set(true);
        ui.controls();
        ui.message("Checking policy…", false);
        let done = ui.clone();
        work(move || super::client::call::<api::PolicyChecked>("_sill/policy.check", api::PolicyCheck { text, fragment: false, root: None }), move |result| {
            done.busy.set(false);
            match result {
                Ok(check) if check.valid => done.message("Policy is valid. Checking does not install or activate it.", false),
                Ok(check) => done.message(&if check.diagnostics.is_empty() { "Policy is invalid".into() } else { check.diagnostics.join("\n") }, true),
                Err(e) => done.message(&format!("Could not check policy: {e}"), true),
            }
            done.controls();
        });
    });
    let weak = Rc::downgrade(&ui);
    ui.import.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(text) = ui.text.borrow().clone() else { return };
        let name = ui.name.text().trim().to_string();
        if name.is_empty() || ui.busy.get() { return; }
        let replace = ui.replace.is_active();
        ui.busy.set(true);
        ui.controls();
        ui.message("Installing policy…", false);
        let done = ui.clone();
        work(move || super::client::call::<api::PolicyEntry>("_sill/policy.import", api::PolicyImport { text, name, replace }), move |result| {
            done.busy.set(false);
            match result {
                Ok(policy) => {
                    done.replace.set_active(false);
                    done.message(&format!("Installed {}. Existing jails keep their launch rules; changes apply on rebuild.", policy.name), false);
                    done.refresh(Some(policy.name));
                }
                Err(e) => done.message(&format!("Could not import policy: {e}"), true),
            }
            done.controls();
        });
    });
    ui.refresh(None);
    // Keep state for this group without making widget signal cycles through the child controls.
    root.connect_destroy(move |_| { let _ = &ui; });
    root.upcast()
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).selectable(true).build()
}
