//! The Changes page's logic (its view and events are `changes_ui.rs`): the listing and the
//! history read, diffs opened, and keep, revert and undo.
use iced::Task;
use serde_json::json;

use crate::{Act, App, Message, blocking, changes_ui, demo};

impl App {
    /// The pending changes and Settings → auto-revert, read off the GUI thread.
    pub(crate) fn changes_load(&self) -> Task<Message> {
        use changes_ui::Event;
        let Some(actions) = self.actions.clone() else {
            return Task::done(Message::Changes(Event::Loaded(Ok((demo::changes(), vec![], 0)))));
        };
        let workspaces = self.workspaces_page_load();
        let changes = blocking(
            move || {
                let (entries, unreadable) = actions
                    .reading(sill_client::changes::listing)
                    .map_err(|e| format!("{e:#}"))?;
                // unread settings: no countdown rather than a wrong one
                let minutes = actions
                    .read::<sill_proto::admin::Settings>("_sill/settings.get", json!({}))
                    .map_or(0, |s| sill_client::changes::auto_revert(&s));
                Ok((entries, unreadable, minutes))
            },
            |loaded| Message::Changes(Event::Loaded(loaded)),
        );
        Task::batch([changes, workspaces])
    }

    fn history_load(&self, before: Option<String>) -> Task<Message> {
        use changes_ui::Event;
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let append = before.is_some();
        blocking(
            move || {
                actions
                    .reading(|c| sill_client::changes::history(c, before.clone()))
                    .map_err(|e| format!("{e:#}"))
            },
            move |page| Message::Changes(Event::HistoryLoaded(page, append)),
        )
    }

    pub(crate) fn changes_event(&mut self, event: changes_ui::Event) -> Task<Message> {
        use changes_ui::{Event, Reverting};
        use sill_client::changes;
        use sill_proto::changes::Entry;
        let demo = self.demo;
        // a listing is the window's: one that arrives after the page closed is kept for the next
        // opening all the same
        let event = match event {
            Event::Loaded(loaded) => {
                let listing = &mut self.changes_listing;
                match loaded {
                    Ok((entries, unreadable, minutes)) => {
                        listing.auto_revert = minutes;
                        listing.entries = Some(Ok(entries));
                        listing.unreadable = unreadable;
                    }
                    Err(error) => listing.entries = Some(Err(error)),
                }
                let Some(page) = &mut self.changes else {
                    return Task::none();
                };
                page.loading = false;
                if let Some(Ok(entries)) = &listing.entries {
                    page.fresh_entries = true;
                    let keys: Vec<String> = entries.iter().map(changes::entry_key).collect();
                    page.open
                        .retain(|k, _| changes_ui::workspace_of(k).is_some() || keys.contains(k));
                    // a revert whose entry went is not waited for (a group's: its first entry)
                    let gone = page.armed.get().is_some_and(|armed| {
                        let key = match armed {
                            Reverting::One(key) => Some(key),
                            Reverting::All(all) => all.first(),
                        };
                        key.is_none_or(|key| !keys.contains(key))
                    });
                    if gone {
                        page.armed.clear();
                    }
                }
                return Task::none();
            }
            event => event,
        };
        let Some(page) = &mut self.changes else {
            return Task::none();
        };
        let listing = &self.changes_listing;
        match event {
            Event::Loaded(_) => {}
            Event::Refresh => {
                if page.history {
                    page.history_loading = true;
                    return self.history_load(None);
                }
                page.loading = true;
                return self.changes_load();
            }
            Event::History(on) => {
                page.history = on;
                if on && page.events.is_empty() && !page.history_loading {
                    page.history_loading = true;
                    return self.history_load(None);
                }
            }
            Event::HistoryLoaded(loaded, append) => {
                page.history_loading = false;
                match loaded {
                    Ok(next) => {
                        if !append {
                            page.events.clear();
                        }
                        page.events.extend(next.events);
                        page.next = next.next;
                        page.history_error = None;
                    }
                    Err(error) => page.history_error = Some(error),
                }
            }
            Event::MoreHistory => {
                let before = page.next.clone();
                page.history_loading = true;
                return self.history_load(before);
            }
            Event::ToggleDiscarded => {
                page.discarded_open = !page.discarded_open;
            }
            Event::Diff(key) => {
                if page.open.remove(&key).is_some() {
                    return Task::none();
                }
                let Some(entry) = listing.entry(&key) else {
                    return Task::none();
                };
                let target = changes::target(entry);
                page.open.insert(key.clone(), None);
                let Some(actions) = self.actions.clone() else {
                    return Task::done(Message::Changes(Event::DiffLoaded(
                        key,
                        Ok(demo::changes_diff()),
                    )));
                };
                return blocking(
                    move || {
                        actions
                            .reading(|c| changes::diff(c, target.clone()))
                            .map_err(|e| format!("{e:#}"))
                    },
                    move |diff| Message::Changes(Event::DiffLoaded(key, diff)),
                );
            }
            Event::WorkspaceDiff(workspace, path) => {
                let key = changes_ui::workspace_key(&workspace, &path);
                if page.open.remove(&key).is_some() {
                    return Task::none();
                }
                page.open.insert(key, None);
                return self.workspace_file_diff(workspace, path);
            }
            Event::DiffLoaded(key, diff) => {
                if let Some(slot) = page.open.get_mut(&key) {
                    *slot = Some(diff);
                }
            }
            event @ (Event::Keep(_)
            | Event::KeepAll(_)
            | Event::Revert(_)
            | Event::RevertAll(_)
            | Event::Undo(_)) => {
                let (keys, armed_as) = match &event {
                    Event::Keep(key) | Event::Undo(key) => (vec![key.clone()], None),
                    Event::KeepAll(keys) => (keys.clone(), None),
                    Event::Revert(key) => (vec![key.clone()], Some(Reverting::One(key.clone()))),
                    // an empty group has nothing to revert and asks nothing
                    Event::RevertAll(keys) => (
                        keys.clone(),
                        (!keys.is_empty()).then(|| Reverting::All(keys.clone())),
                    ),
                    _ => unreachable!(),
                };
                // the listing from before the page was opened is shown, not acted on
                if !page.fresh_entries {
                    return Task::none();
                }
                // reverting takes a second press
                if let Some(armed) = armed_as
                    && !page.armed.press(armed)
                {
                    return Task::none();
                }
                if demo {
                    page.said = Some("The demo keeps and reverts nothing.".into());
                    return Task::none();
                }
                let entries: Vec<&Entry> = keys.iter().filter_map(|k| listing.entry(k)).collect();
                let Some(first) = entries.first() else {
                    return Task::none();
                };
                let (verb, requests) = match (&event, first) {
                    (Event::Revert(_), Entry::Live { repo, path, .. }) => {
                        ("reverted", vec![changes::git_revert(repo, path)])
                    }
                    (
                        Event::Undo(_),
                        Entry::Kept {
                            session_id,
                            keep_id,
                            ..
                        },
                    ) => ("undone", vec![changes::undo(session_id, keep_id)]),
                    // the daemon takes 128 paths a request: more go in several
                    (Event::Keep(_) | Event::KeepAll(_), Entry::Overlay { session_id, .. }) => {
                        ("kept", changes::keep_all(session_id, &entries))
                    }
                    (Event::Revert(_) | Event::RevertAll(_), Entry::Overlay { session_id, .. }) => {
                        ("reverted", changes::revert_all(session_id, &entries))
                    }
                    _ => return Task::none(),
                };
                let Some(actions) = self.actions.clone() else {
                    return Task::none();
                };
                return blocking(
                    move || changes::call_all(&actions, requests),
                    move |outcome| Message::Done(String::new(), Act::Changes(verb), outcome),
                );
            }
        }
        Task::none()
    }
}
