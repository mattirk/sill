//! Policy library in Settings; requests run off the UI thread and imports are never retried.
use crate::settings_ui::quiet_button;
use crate::{App, Message, settings_ui, style};
use iced::widget::{column, pick_list, row, scrollable, text, text_input};
use iced::{Element, Fill, Task};
use sill_proto::admin as api;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
pub struct State {
    token: u64,
    pub loaded: bool,
    pub busy: bool,
    pub names: Vec<String>,
    selected: Option<String>,
    path: String,
    name: String,
    preview: String,
    source: String,
    file: bool,
    valid: bool,
    replace: bool,
    pub status: String,
}
impl Default for State {
    fn default() -> Self {
        Self {
            token: NEXT.fetch_add(1, Ordering::Relaxed),
            loaded: false,
            busy: false,
            names: vec![],
            selected: None,
            path: String::new(),
            name: String::new(),
            preview: String::new(),
            source: String::new(),
            file: false,
            valid: false,
            replace: false,
            status: String::new(),
        }
    }
}
#[derive(Debug, Clone)]
pub enum Event {
    Refresh,
    Inspect(String),
    Path(String),
    Name(String),
    Replace(bool),
    Load,
    Check,
    Import,
    Done(u64, Result<Reply, String>),
}
#[derive(Debug, Clone)]
pub enum Reply {
    Names(Vec<String>),
    Entry(api::PolicyEntry),
    File { text: String, path: String, name: String },
    Checked(api::PolicyChecked),
    Imported(api::PolicyEntry),
}
fn event(e: Event) -> Message {
    Message::Settings(settings_ui::Event::Policy(e))
}
impl State {
    pub fn view(&self) -> Element<'_, Message> {
        let mut names = pick_list(self.names.clone(), self.selected.clone(), |n| event(Event::Inspect(n))).placeholder("Select installed policy").width(Fill);
        // Busy events are also rejected by the update handler.
        if self.busy {
            names = names.placeholder("Working…");
        }
        let mut path = text_input("Absolute path to a local TOML file", &self.path).padding(7);
        let mut name = text_input("Import as policy name", &self.name).padding(7);
        if !self.busy {
            path = path.on_input(|s| event(Event::Path(s))).on_submit(event(Event::Load));
            name = name.on_input(|s| event(Event::Name(s)));
        }
        let replace =
            iced::widget::checkbox(self.replace).label("Replace an existing user policy").on_toggle_maybe((!self.busy).then_some(|v| event(Event::Replace(v))));
        column![
            row![names, quiet_button("Refresh", (!self.busy).then(|| event(Event::Refresh)))].spacing(8),
            text(&self.source).size(style::SMALL),
            scrollable(text(&self.preview).font(iced::Font::MONOSPACE).size(style::SMALL))
                .height(220).id("policy-preview"),
            text("Load a complete TOML policy from this computer. Check validates without installing. Import installs on the connected Sill host; existing jails keep their launch rules until rebuilt.").size(style::SMALL),
            row![path, quiet_button("Load", (!self.busy && !self.path.trim().is_empty()).then(|| event(Event::Load)))].spacing(8),
            name, replace,
            row![
                quiet_button("Check", (!self.busy && self.file).then(|| event(Event::Check))),
                quiet_button(if self.replace { "Replace policy" } else { "Import policy" },
                    self.can_import().then(|| event(Event::Import))),
            ].spacing(8),
        ].spacing(10).into()
    }
    fn can_import(&self) -> bool {
        !self.busy && self.file && self.valid && !self.name.trim().is_empty()
    }
}

fn read_file(path: String) -> Result<Reply, String> {
    use std::io::Read;
    let p = std::path::Path::new(path.trim());
    if !p.is_absolute() {
        return Err("Enter an absolute path on this computer.".into());
    }
    let file = std::fs::File::open(p).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Choose a regular file.".into());
    }
    let mut text = String::new();
    file.take(api::TEXT_BYTES as u64 + 1).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > api::TEXT_BYTES {
        return Err("Policy file exceeds 1 MiB.".into());
    }
    Ok(Reply::File { text, name: p.file_stem().and_then(|s| s.to_str()).unwrap_or("").into(), path: p.display().to_string() })
}

impl App {
    pub(crate) fn policy_event(&mut self, e: Event) -> Task<Message> {
        let Some(page) = &mut self.settings_page else {
            return Task::none();
        };
        let state = &mut page.policies;
        if let Event::Done(token, result) = e {
            if token != state.token {
                return Task::none();
            }
            state.busy = false;
            match result {
                Err(why) => state.status = why,
                Ok(Reply::Names(names)) => {
                    state.names = names;
                    state.loaded = true;
                    state.status = format!("{} policies installed.", state.names.len());
                }
                Ok(Reply::Entry(entry)) => {
                    state.source = format!("Installed base policy: {}", entry.name);
                    state.selected = Some(entry.name);
                    state.preview = entry.text;
                    state.status = format!("{} policies installed.", state.names.len());
                }
                Ok(Reply::File { text, path, name }) => {
                    state.preview = text;
                    state.source = format!("Local file: {path}");
                    state.name = name;
                    state.file = true;
                    state.status = "File loaded. Review and Check before importing.".into();
                }
                Ok(Reply::Checked(check)) => {
                    state.valid = check.valid;
                    state.status = if check.valid {
                        "Policy is valid. Nothing has been installed or activated.".into()
                    } else if check.diagnostics.is_empty() {
                        "Policy is invalid.".into()
                    } else {
                        check.diagnostics.join("\n")
                    };
                }
                Ok(Reply::Imported(entry)) => {
                    if !state.names.contains(&entry.name) {
                        state.names.push(entry.name.clone());
                        state.names.sort();
                    }
                    state.status = format!("Installed {}. Existing jails keep their launch rules until rebuilt.", entry.name);
                    state.source = format!("Installed base policy: {}", entry.name);
                    state.selected = Some(entry.name);
                    state.preview = entry.text;
                    state.replace = false;
                    state.file = false;
                    state.valid = false;
                }
            }
            return Task::none();
        }
        if state.busy {
            return Task::none();
        }
        match &e {
            Event::Path(s) => {
                state.path = s.clone();
                state.file = false;
                state.valid = false;
                state.replace = false;
                return Task::none();
            }
            Event::Name(s) => {
                state.name = s.clone();
                state.replace = false;
                return Task::none();
            }
            Event::Replace(v) => {
                state.replace = *v;
                return Task::none();
            }
            Event::Check if !state.file => return Task::none(),
            Event::Import if !state.can_import() => return Task::none(),
            Event::Load | Event::Inspect(_) => {
                state.file = false;
                state.valid = false;
                state.replace = false;
                state.preview.clear();
                state.source.clear();
                state.selected = None;
            }
            _ => {}
        }
        state.busy = true;
        state.status = "Working…".into();
        let token = state.token;
        let path = state.path.clone();
        let text = state.preview.clone();
        let name = state.name.trim().to_string();
        let replace = state.replace;
        let actions = self.actions.clone();
        crate::blocking(
            move || -> Result<Reply, String> {
                if matches!(e, Event::Load) {
                    return read_file(path);
                }
                let actions = actions.ok_or("The demo has no policy daemon.")?;
                match e {
                    Event::Refresh => actions
                        .reading(|client| {
                            let mut names = Vec::new();
                            let mut after = None;
                            loop {
                                let page: api::Names = client.call("_sill/policy.list", api::NamesList { after: after.clone(), limit: Some(128) })?;
                                names.extend(page.names);
                                anyhow::ensure!(names.len() <= 4096, "Too many policies to display");
                                let Some(next) = page.next else {
                                    break;
                                };
                                anyhow::ensure!(after.as_ref().is_none_or(|last| &next > last), "Invalid policy list cursor");
                                after = Some(next);
                            }
                            Ok(Reply::Names(names))
                        })
                        .map_err(|e| format!("{e:#}")),
                    Event::Inspect(name) => {
                        actions.read::<api::PolicyEntry>("_sill/policy.get", serde_json::json!({"name": name})).map(Reply::Entry).map_err(|e| format!("{e:#}"))
                    }
                    Event::Check => actions
                        .read::<api::PolicyChecked>("_sill/policy.check", serde_json::to_value(api::PolicyCheck { text }).unwrap())
                        .map(Reply::Checked)
                        .map_err(|e| format!("{e:#}")),
                    Event::Import => match actions.call("_sill/policy.import", serde_json::to_value(api::PolicyImport { name, text, replace }).unwrap()) {
                        sill_client::actions::Outcome::Done(v) => serde_json::from_value(v)
                            .map(Reply::Imported)
                            .map_err(|e| format!("Import response unreadable; refresh and inspect before retrying: {e}")),
                        sill_client::actions::Outcome::Uncertain(why) => Err(format!("Import outcome unknown. Refresh and inspect before retrying. {why}")),
                        other => Err(other.problem().unwrap_or("Import failed").into()),
                    },
                    _ => unreachable!(),
                }
            },
            move |r| event(Event::Done(token, r)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let (mut app, _) = App::new(true, None);
        app.settings_page = Some(settings_ui::SettingsPage::default());
        app
    }
    fn file(app: &mut App) {
        let token = app.settings_page.as_ref().unwrap().policies.token;
        let _ = app.policy_event(Event::Done(token, Ok(Reply::File { text: "policy text".into(), path: "/tmp/policy.toml".into(), name: "reviewed".into() })));
    }
    #[test]
    fn import_requires_checked_file_and_name_change_clears_replacement() {
        let mut app = app();
        file(&mut app);
        let state = &app.settings_page.as_ref().unwrap().policies;
        assert!(!state.can_import());
        let token = state.token;
        let _ = app.policy_event(Event::Import);
        assert!(!app.settings_page.as_ref().unwrap().policies.busy);
        let _ = app.policy_event(Event::Done(token, Ok(Reply::Checked(api::PolicyChecked { valid: true, diagnostics: vec![] }))));
        assert!(app.settings_page.as_ref().unwrap().policies.can_import());
        let _ = app.policy_event(Event::Replace(true));
        let _ = app.policy_event(Event::Name("different".into()));
        assert!(!app.settings_page.as_ref().unwrap().policies.replace);
        let _ = app.policy_event(Event::Path("/tmp/another.toml".into()));
        assert!(!app.settings_page.as_ref().unwrap().policies.can_import());
    }
    #[test]
    fn stale_reply_cannot_populate_reopened_settings_and_busy_input_is_ignored() {
        let mut app = app();
        let old = app.settings_page.as_ref().unwrap().policies.token;
        app.settings_page = Some(settings_ui::SettingsPage::default());
        let _ = app.policy_event(Event::Done(old, Ok(Reply::Names(vec!["stale".into()]))));
        assert!(app.settings_page.as_ref().unwrap().policies.names.is_empty());
        let _ = app.policy_event(Event::Refresh);
        let _ = app.policy_event(Event::Name("changed during request".into()));
        assert!(app.settings_page.as_ref().unwrap().policies.name.is_empty());
    }
    #[test]
    fn invalid_check_and_errors_survive_collapsing() {
        let mut app = app();
        file(&mut app);
        let token = app.settings_page.as_ref().unwrap().policies.token;
        let _ = app.policy_event(Event::Done(token, Ok(Reply::Checked(api::PolicyChecked { valid: false, diagnostics: vec!["Invalid TOML".into()] }))));
        let page = app.settings_page.as_ref().unwrap();
        assert!(page.collapsed.contains("Policies"));
        assert_eq!(page.policies.status, "Invalid TOML");
        assert!(!page.policies.can_import());
    }
    #[test]
    fn file_reader_rejects_relative_paths_and_oversize_files() {
        assert!(read_file("relative.toml".into()).is_err());
        let path = std::env::temp_dir().join(format!("sill-policy-{}-{}.toml", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, vec![b'x'; api::TEXT_BYTES + 1]).unwrap();
        assert!(read_file(path.display().to_string()).unwrap_err().contains("1 MiB"));
        std::fs::write(&path, "[policy]\n").unwrap();
        assert!(matches!(read_file(path.display().to_string()), Ok(Reply::File { text, .. }) if text == "[policy]\n"));
        std::fs::remove_file(path).unwrap();
    }
}
