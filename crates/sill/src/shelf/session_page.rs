//! A session's page (docs/memory.md §2.5), shown in the shelf's viewer: the facts in one row
//! (agent, when, how long, what it kept, its folders), the summary — editable, saved as it is
//! typed and marked "edited by the person" — *Summarize now* while the session runs (the agent
//! writes a checkpoint summary; the end's summary replaces it), and the notes: memory entries the
//! person adds (one line each, Enter adds; × removes). Built anew each time it is opened.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use sill_core::store::{Session, Store};

use super::session_view::SessionView;

/// Typing pauses this long before the summary is written to the store.
const SAVE_AFTER_MS: u64 = 600;

/// The page for `s`; `view` is its open tab, if any (what *Summarize now* asks); `changed` runs
/// after every write to the store (the owner refreshes memory's snapshot).
pub fn build(store: Rc<Store>, s: &Session, view: Option<Rc<SessionView>>, changed: Rc<dyn Fn()>) -> gtk::Box {
    let home = super::session::home().unwrap_or_default();
    let id = s.id.clone();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 4);
    page.add_css_class("sill-page");

    // the facts: one row, selectable
    let title = gtk::Label::new(Some(&s.title));
    title.add_css_class("sill-page-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_selectable(true);
    page.append(&title);
    let now = sill_core::store::now();
    let kept = sill_core::memory::kept_of(&id);
    let pins = store.pins(&id).unwrap_or_default();
    let mut facts = vec![s.agent.clone(), format!("started {}", sill_memory::datetime(s.created_at))];
    match s.ended_at {
        Some(t) => facts.push(format!("ended {} · {}", sill_memory::datetime(t), sill_memory::duration(t - s.created_at))),
        None => facts.push(format!("open · {}", sill_memory::duration(now - s.created_at))),
    }
    if !kept.is_empty() {
        facts.push(format!("kept {} path{}", kept.len(), if kept.len() == 1 { "" } else { "s" }));
    }
    if !pins.is_empty() {
        facts.push(pins.iter().map(|p| sill_memory::tilde(p, &home)).collect::<Vec<_>>().join(", "));
    }
    if s.compacted > 0 {
        facts.push(format!("transcript compacted ({})", s.compacted));
    }
    let meta = gtk::Label::new(Some(&facts.join(" · ")));
    meta.add_css_class("sill-page-meta");
    meta.set_xalign(0.0);
    meta.set_wrap(true);
    meta.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    meta.set_selectable(true);
    page.append(&meta);

    // the summary: a heading with who wrote it, the text editable in place, Summarize now
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let h = gtk::Label::new(Some("Summary"));
    h.add_css_class("sill-page-head");
    h.set_xalign(0.0);
    head.append(&h);
    let by = gtk::Label::new(None);
    by.add_css_class("sill-page-caption");
    by.set_xalign(0.0);
    by.set_hexpand(true);
    by.set_valign(gtk::Align::End);
    head.append(&by);
    let set_by = {
        let by = by.clone();
        move |kind: &str, empty: bool| {
            by.set_text(if empty { "no summary yet" } else { sill_memory::summary_by(kind) });
        }
    };
    set_by(&s.summary_kind, s.summary.trim().is_empty());
    let summarize = gtk::Button::with_label("Summarize now");
    summarize.add_css_class("flat");
    summarize.add_css_class("sill-viewer-btn");
    summarize.set_valign(gtk::Align::End);
    summarize.set_margin_top(14);
    summarize.set_visible(view.is_some());
    head.append(&summarize);
    page.append(&head);

    let buf = gtk::TextBuffer::new(None);
    buf.set_text(&s.summary);
    let text = gtk::TextView::with_buffer(&buf);
    text.add_css_class("sill-page-text");
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.set_accepts_tab(false);
    text.set_top_margin(4);
    text.set_bottom_margin(4);
    text.set_left_margin(4);
    text.set_right_margin(4);
    page.append(&text);

    // saved as typed, a moment after the last keystroke; a checkpoint's arrival sets the text
    // too, so writes from the agent are told apart from the person's by `from_agent`
    let stored = Rc::new(RefCell::new(s.summary.clone()));
    let from_agent = Rc::new(Cell::new(false));
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    {
        let (store, id, stored, from_agent, pending, changed, set_by) = (store.clone(), id.clone(), stored.clone(), from_agent.clone(), pending.clone(), changed.clone(), set_by.clone());
        buf.connect_changed(move |b| {
            if from_agent.get() {
                return;
            }
            if let Some(t) = pending.borrow_mut().take() {
                t.remove();
            }
            let b = b.clone();
            let (store, id, stored, pending2, changed, set_by) = (store.clone(), id.clone(), stored.clone(), pending.clone(), changed.clone(), set_by.clone());
            let src = glib::timeout_add_local_once(std::time::Duration::from_millis(SAVE_AFTER_MS), move || {
                pending2.borrow_mut().take();
                let now_text = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
                if now_text == *stored.borrow() {
                    return;
                }
                if store.set_summary(&id, &now_text, "person").is_ok() {
                    *stored.borrow_mut() = now_text.clone();
                    set_by("person", now_text.trim().is_empty());
                    changed();
                }
            });
            *pending.borrow_mut() = Some(src);
        });
    }

    // Summarize now: the agent's checkpoint replaces the text (and what the person typed since
    // opening the page — the agent saw the whole session, the text box did not)
    if let Some(v) = view.as_ref() {
        let weak: Weak<SessionView> = Rc::downgrade(v);
        let (store, id, stored, from_agent, buf, changed, set_by, home) = (store.clone(), id.clone(), stored.clone(), from_agent.clone(), buf.clone(), changed.clone(), set_by.clone(), home.clone());
        let refresh_state = {
            let weak = weak.clone();
            let b = summarize.clone();
            move || {
                let Some(v) = weak.upgrade() else {
                    b.set_visible(false);
                    return;
                };
                if v.summary_pending() {
                    b.set_sensitive(false);
                    b.set_tooltip_text(Some("The agent is writing a summary"));
                } else if v.agent_idle() {
                    b.set_sensitive(true);
                    b.set_tooltip_text(Some("Ask the agent for a summary of the session so far: it replaces the text above; the one written when the session ends replaces that"));
                } else {
                    b.set_sensitive(false);
                    b.set_tooltip_text(Some("The agent is busy or not running"));
                }
            }
        };
        refresh_state();
        {
            // the button follows the agent's state while the page is shown; the timer ends with
            // the page (the viewer drops it when another is opened or it closes)
            let refresh_state = refresh_state.clone();
            let b = summarize.clone();
            let shown = Cell::new(false);
            glib::timeout_add_seconds_local(2, move || {
                let in_window = b.root().is_some();
                if shown.get() && !in_window {
                    return glib::ControlFlow::Break;
                }
                shown.set(shown.get() || in_window);
                refresh_state();
                glib::ControlFlow::Continue
            });
        }
        summarize.connect_clicked(move |b| {
            let Some(v) = weak.upgrade() else { return };
            let Some(sess) = store.sessions().unwrap_or_default().into_iter().find(|x| x.id == id) else { return };
            let kept_dir = sill_core::session::state_dir(super::session::uid(), &id).map(|d| d.join("kept")).unwrap_or_default();
            let facts = sill_core::memory::collect(&store, &sess, &kept_dir);
            let prompt = sill_core::memory::checkpoint_prompt(&facts, &home);
            b.set_sensitive(false);
            b.set_label("writing…");
            let (store, id, stored, from_agent, buf, changed, set_by, b2, refresh_state) = (store.clone(), id.clone(), stored.clone(), from_agent.clone(), buf.clone(), changed.clone(), set_by.clone(), b.clone(), refresh_state.clone());
            v.ask_checkpoint(&prompt, Box::new(move |answer| {
                b2.set_label("Summarize now");
                refresh_state();
                let Some(a) = answer.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) else { return };
                if store.set_summary(&id, &a, "agent").is_ok() {
                    *stored.borrow_mut() = a.clone();
                    from_agent.set(true);
                    buf.set_text(&a);
                    from_agent.set(false);
                    set_by("agent", false);
                    changed();
                }
            }));
        });
    }

    // the notes: the person's memory entries, newest last, one line each
    let nh = gtk::Label::new(Some("Notes"));
    nh.add_css_class("sill-page-head");
    nh.set_xalign(0.0);
    page.append(&nh);
    let cap = gtk::Label::new(Some("What the summary should have said, or what the next agent here must know. Enter adds a note."));
    cap.add_css_class("sill-page-caption");
    cap.set_xalign(0.0);
    cap.set_wrap(true);
    page.append(&cap);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&list);
    let add_row = {
        let (store, id, list, changed) = (store.clone(), id.clone(), list.clone(), changed.clone());
        Rc::new(move |ts: i64, note: String| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("sill-page-note");
            let when = gtk::Label::new(Some(&sill_memory::datetime(ts)));
            when.add_css_class("sill-page-caption");
            when.set_valign(gtk::Align::Start);
            let l = gtk::Label::new(Some(&note));
            l.set_xalign(0.0);
            l.set_hexpand(true);
            l.set_wrap(true);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_selectable(true);
            let x = super::icons::glyph_button("sill-x");
            x.add_css_class("flat");
            x.add_css_class("sill-tab-close");
            x.set_valign(gtk::Align::Start);
            x.set_tooltip_text(Some("Remove this note"));
            {
                let (store, id, list, row, changed) = (store.clone(), id.clone(), list.clone(), row.clone(), changed.clone());
                x.connect_clicked(move |_| {
                    if store.remove_memory(&id, ts, &note).is_ok() {
                        list.remove(&row);
                        changed();
                    }
                });
            }
            row.append(&when);
            row.append(&l);
            row.append(&x);
            list.append(&row);
        })
    };
    for (ts, kind, text) in store.memories(&id).unwrap_or_default() {
        if kind == "note" {
            add_row(ts, text);
        }
    }
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("a note on this session"));
    entry.set_margin_top(6);
    {
        let (store, id, add_row, changed) = (store.clone(), id.clone(), add_row.clone(), changed.clone());
        entry.connect_activate(move |e| {
            let text = e.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            if store.add_memory(&id, "note", &text).is_ok() {
                add_row(sill_core::store::now(), text);
                e.set_text("");
                changed();
            }
        });
    }
    page.append(&entry);
    page
}
