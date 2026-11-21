//! The Settings page's logic (its view and events are `settings_ui.rs`): the daemon's settings
//! read and changed, and silli-gui's own.
use iced::Task;
use iced::widget::{operation, scrollable, text_editor};
use silli_client::actions::Outcome;
use silli_proto::method;

use crate::{App, Message, blocking, daemon_ui, geometry, keys, layout, lists, policies, settings_ui, style};

impl App {
    pub(crate) fn read_daemon_settings(&mut self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        if self.settings_pending {
            return Task::none();
        }
        self.settings_pending = true;
        let revision = self.settings_revision;
        blocking(
            move || {
                actions.reading(|client| client.call::<silli_proto::admin::Settings>(method::SETTINGS_GET, silli_proto::Empty {})).map_err(|e| format!("{e:#}"))
            },
            move |result| Message::DaemonSettings(revision, result),
        )
    }

    pub(crate) fn settings_load(&mut self) -> Task<Message> {
        let Some(page) = self.settings_page.as_mut() else {
            return Task::none();
        };
        let token = page.token.clone();
        let load = silli_client::request_id("settings-load");
        page.load_token = load.clone();
        if !page.autostart_busy {
            page.autostart = silli_proto::autostart_enabled().map_err(|e| e.to_string());
        }
        let Some(actions) = self.actions.clone() else {
            // the demo: the defaults, as a daemon that was never configured has them
            let snapshot = silli_client::settings::Snapshot {
                settings: silli_proto::admin::Settings { theme: style::theme_id().into(), ..Default::default() },
                instructions: "Commit after the person has tested a feature.".into(),
                global_pins: vec![],
                active_hours: Some(crate::usage::demo()),
            };
            return Task::done(Message::SettingsReply(token, settings_ui::Event::Loaded(load, Box::new(Ok(snapshot)))));
        };
        blocking(
            move || actions.reading(silli_client::settings::read).map_err(|e| format!("{e:#}")),
            move |r| Message::SettingsReply(token, settings_ui::Event::Loaded(load, Box::new(r))),
        )
    }

    /// One request of the Settings page; `said` tells what it did, from its answer.
    fn settings_call(&self, (method, params): silli_client::settings::Request, said: fn(&serde_json::Value) -> String) -> Task<Message> {
        let Some(token) = self.settings_page.as_ref().map(|p| p.token.clone()) else {
            return Task::none();
        };
        let Some(actions) = self.actions.clone() else {
            return Task::done(Message::SettingsReply(token, settings_ui::Event::Done(Err("The demo has no daemon: settings are examples here.".into()))));
        };
        blocking(
            move || match actions.call(method, params) {
                silli_client::actions::Outcome::Done(value) => Ok(said(&value)),
                other => Err(other.problem().unwrap_or("failed").to_string()),
            },
            move |r| Message::SettingsReply(token, settings_ui::Event::Done(r)),
        )
    }

    pub(crate) fn settings_event(&mut self, event: settings_ui::Event) -> Task<Message> {
        use settings_ui::Event;
        use silli_client::settings;
        if let Event::Policy(event) = event {
            return self.policy_event(event);
        }
        let Some(page) = &mut self.settings_page else {
            return Task::none();
        };
        // anything else the page is asked to do ends the recording of a shortcut; answers that
        // come in from the daemon meanwhile do not
        let recording = page.recording;
        if !matches!(event, Event::Loaded(..) | Event::Done(_) | Event::AutostartDone(_) | Event::InstructionsSaved(..) | Event::Scrolled(..)) {
            page.recording = None;
            if !matches!(event, Event::ShortcutKey(..)) {
                page.shortcut_refused = None;
            }
        }
        match event {
            Event::Autostart(enabled) => {
                if page.autostart_busy || self.demo || !silli_proto::transport::is_local() {
                    return Task::none();
                }
                page.autostart_busy = true;
                page.said = None;
                let token = page.token.clone();
                return blocking(move || crate::daemon_ui::set_autostart(enabled), move |result| Message::SettingsReply(token, Event::AutostartDone(result)));
            }
            Event::AutostartDone(result) => {
                page.autostart_busy = false;
                page.autostart = silli_proto::autostart_enabled().map_err(|e| e.to_string());
                page.said = Some(match result {
                    Ok(()) => "Daemon autostart preference updated.".into(),
                    Err(why) => format!("Autostart change failed: {why}"),
                });
                return self.daemon_event(daemon_ui::Event::Check);
            }
            Event::Category(category) => {
                page.category = category;
                page.armed_delete.clear();
                self.settings.settings_category = category;
                // only the person's own window writes their file: never the demo or a test
                if self.persist {
                    crate::settings::save(&self.settings);
                }
                return operation::scroll_to("settings", scrollable::AbsoluteOffset { x: 0.0, y: page.scroll_offsets.get(&category).copied().unwrap_or(0.0) });
            }
            Event::Scrolled(category, offset) => {
                page.scroll_offsets.insert(category, offset);
            }
            Event::ToggleSection(title) => {
                if !page.collapsed.remove(title) {
                    page.collapsed.insert(title);
                }
                if title == "Policies" && !page.collapsed.contains(title) && !page.policies.loaded && !page.policies.busy {
                    return self.policy_event(policies::Event::Refresh);
                }
            }
            Event::Policy(_) => unreachable!(),
            Event::Loaded(load, loaded) => {
                if page.load_token != load {
                    return Task::none();
                }
                let loaded = *loaded;
                if let Ok(snapshot) = &loaded {
                    // An older background settings.get must not undo this page's fresh read.
                    self.settings_revision += 1;
                    style::set_theme(&snapshot.settings.theme);
                    self.daemon_settings = Some(snapshot.settings.clone());
                    if !page.edited {
                        page.instructions = text_editor::Content::with_text(&snapshot.instructions);
                    } else if page.instructions_saving.is_none() && page.instructions.text() == snapshot.instructions {
                        page.edited = false;
                    }
                }
                page.snapshot = Some(loaded);
                self.sync_drawer_follow();
            }
            Event::Change(change) => {
                page.armed_delete.clear();
                // the theme shows at once; the rest after the daemon has it
                if let silli_proto::admin::SettingChange::Theme { value } = &change {
                    style::set_theme(value);
                }
                return self.settings_call(settings::patch(change), |_| String::new());
            }
            Event::Done(result) => {
                page.said = match result {
                    Ok(said) => (!said.is_empty()).then_some(said),
                    Err(why) => Some(format!("Not changed: {why}")),
                };
                return self.settings_load();
            }
            Event::RecordShortcut(id) => {
                page.shortcut_refused = None;
                page.recording = (recording != Some(id)).then_some(id);
            }
            Event::ShortcutKey(key, physical, modifiers) => {
                use crate::shortcuts::Capture;
                let Some(id) = recording else {
                    return Task::none();
                };
                if id == settings_ui::HOTKEY {
                    match crate::hotkey::capture(&key, physical, modifiers) {
                        Capture::Waiting => page.recording = recording,
                        Capture::Refused(why) => {
                            page.recording = recording;
                            page.shortcut_refused = Some(why);
                        }
                        Capture::Cancel => {}
                        // the grab follows from `sync_hotkey`, as it does when recording ends
                        Capture::Clear => self.settings.hotkey = Some(String::new()),
                        Capture::Chord(spec) => self.settings.hotkey = Some(spec),
                    }
                    self.save_settings();
                    return Task::none();
                }
                let binding = match crate::shortcuts::capture(&key, physical, modifiers) {
                    // the chord is not complete, or was not one: go on recording
                    Capture::Waiting => {
                        page.recording = recording;
                        return Task::none();
                    }
                    Capture::Refused(why) => {
                        page.recording = recording;
                        page.shortcut_refused = Some(why);
                        return Task::none();
                    }
                    Capture::Cancel => return Task::none(),
                    Capture::Clear => String::new(),
                    Capture::Chord(binding) => binding,
                };
                let mut bindings = self.settings.shortcuts.clone();
                bindings.insert(id.into(), binding);
                match crate::shortcuts::validate(&bindings) {
                    Ok(()) => {
                        page.shortcut_refused = None;
                        self.settings.shortcuts = bindings;
                        self.save_settings();
                    }
                    Err(why) => {
                        page.recording = recording;
                        page.shortcut_refused = Some(why);
                    }
                }
            }
            Event::ResetShortcuts => {
                self.settings.shortcuts.clear();
                self.save_settings();
            }
            Event::Scale(scale) => return self.set_scale(scale),
            Event::AlwaysApply(enabled) => {
                self.settings.always_apply = enabled;
                self.save_settings();
            }
            Event::ShelfShare(share) => {
                // the main display's width, and the default for displays without their own
                self.settings.shelf_width = Some(share);
                if let Some(main) = geometry::main_display(&self.geometry.displays) {
                    let prefs = self.settings.displays.entry(main.name.clone()).or_default();
                    prefs.shelf_width = Some(share);
                }
                self.save_settings();
                if self.layout.mode == layout::Mode::Shelf {
                    return self.place();
                }
            }
            Event::Instructions(action) => {
                if action.is_edit() {
                    page.edited = true;
                }
                page.instructions.perform(action);
            }
            Event::InstructionsNewline => {
                page.edited = true;
                lists::newline(&mut page.instructions);
            }
            Event::InstructionsIndent(outdent) => {
                let before = page.instructions.text();
                if !lists::indent(&mut page.instructions, outdent) {
                    return keys::step(!outdent);
                }
                page.edited |= page.instructions.text() != before;
            }
            Event::SaveInstructions => {
                if page.instructions_saving.is_some() {
                    return Task::none();
                }
                let text = page.instructions.text();
                page.instructions_saving = Some(text.clone());
                let token = page.token.clone();
                let actions = self.actions.clone();
                return blocking(
                    {
                        let text = text.clone();
                        move || match actions {
                            Some(actions) => {
                                let (method, params) = settings::set_instructions(&text);
                                actions.call(method, params)
                            }
                            None => Outcome::NotSent("The demo has no daemon: settings are examples here.".into()),
                        }
                    },
                    move |outcome| Message::SettingsReply(token, Event::InstructionsSaved(text, outcome)),
                );
            }
            Event::InstructionsSaved(text, outcome) => {
                if page.instructions_saving.as_ref() != Some(&text) {
                    return Task::none();
                }
                page.instructions_saving = None;
                page.said = Some(match outcome.problem() {
                    Some(why) => format!("Not changed: {why}"),
                    None => "Instructions saved: agents get them when they start or next look".into(),
                });
                // Keep edits protected until a read confirms the current text. A failed save,
                // an old snapshot or new typing during the request must not replace them.
                return self.settings_load();
            }
            Event::PinPath(value) => page.pin_path = value,
            Event::AddPin => {
                let path = page.pin_path.trim().to_string();
                if !path.starts_with('/') {
                    return Task::none();
                }
                page.pin_path.clear();
                let path = silli_proto::sessions::HostPath::from_bytes(path.as_bytes());
                return self.settings_call(settings::add_global_pin(path), |_| "Pinned for every new session".into());
            }
            Event::RemovePin(path) => {
                return self.settings_call(settings::remove_global_pin(path), |_| String::new());
            }
            Event::RestoreNetworkPrompts => {
                return self.settings_call(settings::patch(silli_proto::admin::SettingChange::NetworkQuietHosts { value: vec![] }), |_| {
                    "Network prompts restored. Network access is unchanged.".into()
                });
            }
            Event::Compact => {
                page.said = Some("Compacting…".into());
                return self.settings_call(settings::compact(), |v| {
                    let n = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("0").to_string();
                    format!("Compacted {} · {} trimmed · {} deleted", n("compacted"), n("trimmed"), n("deleted"))
                });
            }
            Event::DeleteAll => {
                if !page.armed_delete.press(()) {
                    return Task::none();
                }
                return self.settings_call(settings::forget_ended(), |v| {
                    format!("Deleted {} ended sessions", v.get("deleted").and_then(|x| x.as_str()).unwrap_or("0"))
                });
            }
            Event::ToggleWhere => page.where_open = !page.where_open,
            Event::AlwaysOnTop(on) => return self.set_always_on_top(on),
            Event::ResetHotkey => {
                self.settings.hotkey = None;
                self.save_settings();
            }
        }
        Task::none()
    }
}
