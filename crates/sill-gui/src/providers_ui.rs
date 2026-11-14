//! Model providers on the Agents page (docs/model-providers.md): a section in the expanded row of
//! each agent the daemon hands providers to, with the same list in each, and one line under the
//! catalogue for people who come looking. Nothing here is where a new user starts.
use crate::{
    App, Message, agents_ui, keys,
    style::{self, P},
};
use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input};
use iced::{Alignment, Element, Fill, Task};
use sill_client::providers;
use sill_proto::providers as api;
use std::collections::BTreeSet;

/// How many unticked models the form lists at once; the filter narrows the rest.
const SHOWN_MODELS: usize = 24;
const AGENT_CHOOSES: &str = "Let the agent choose";

#[derive(Default)]
pub struct Providers {
    /// The daemon answered `sill.providers`.
    supported: bool,
    entries: Vec<api::Entry>,
    /// Agents the daemon hands providers to.
    agents: Vec<String>,
    note: Option<String>,
    form: Option<Form>,
    /// A Remove waiting for its second press.
    armed: Option<String>,
    busy: bool,
}

/// What the form fills in. Presets of one type of endpoint, not types of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    OpenRouter,
    Ollama,
    LmStudio,
    Other,
}
impl Kind {
    const ALL: [Kind; 4] = [Kind::OpenRouter, Kind::Ollama, Kind::LmStudio, Kind::Other];
    fn name(self) -> &'static str {
        match self {
            Kind::OpenRouter => "OpenRouter",
            Kind::Ollama => "Ollama",
            Kind::LmStudio => "LM Studio",
            Kind::Other => "",
        }
    }
    fn address(self) -> &'static str {
        match self {
            Kind::OpenRouter => "https://openrouter.ai/api/v1",
            Kind::Ollama => "http://192.168.1.20:11434/v1",
            Kind::LmStudio => "http://192.168.1.20:1234/v1",
            Kind::Other => "",
        }
    }
}
impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Other => "Other (OpenAI-compatible)",
            kind => kind.name(),
        })
    }
}

struct Form {
    /// The id of the provider being edited; a new one gets its id from its name.
    editing: Option<String>,
    kind: Kind,
    name: String,
    address: String,
    key: String,
    had_key: bool,
    clear_key: bool,
    /// Every model known here: found by Test, saved, or typed. Sorted.
    models: Vec<String>,
    ticked: BTreeSet<String>,
    /// Narrows the list; Add takes it as a model id the endpoint did not list.
    model_input: String,
    default_model: Option<String>,
    testing: bool,
    saving: bool,
    status: Option<Result<String, String>>,
}
impl Form {
    fn new() -> Self {
        let kind = Kind::default();
        Form {
            editing: None,
            kind,
            name: kind.name().into(),
            address: kind.address().into(),
            key: String::new(),
            had_key: false,
            clear_key: false,
            models: vec![],
            ticked: BTreeSet::new(),
            model_input: String::new(),
            default_model: None,
            testing: false,
            saving: false,
            status: None,
        }
    }
    fn of(entry: &api::Entry) -> Self {
        let p = &entry.provider;
        Form {
            editing: Some(p.id.clone()),
            kind: Kind::Other,
            name: p.name.clone(),
            address: p.base_url.clone(),
            had_key: entry.has_key,
            models: p.models.clone(),
            ticked: p.models.iter().cloned().collect(),
            default_model: p.default_model.clone(),
            ..Form::new()
        }
    }
    fn know(&mut self, models: impl IntoIterator<Item = String>) {
        self.models.extend(models);
        self.models.sort();
        self.models.dedup();
    }
    fn can_save(&self) -> bool {
        !self.saving && !self.name.trim().is_empty() && !self.address.trim().is_empty() && !self.ticked.is_empty()
    }
    /// The record the form describes, with an id not among `taken` for a new one.
    fn provider(&self, taken: &[&str]) -> api::Provider {
        api::Provider {
            id: self.editing.clone().unwrap_or_else(|| providers::id_for(&self.name, taken)),
            name: self.name.trim().into(),
            api: api::Api::Openai,
            base_url: self.address.trim().into(),
            models: self.ticked.iter().cloned().collect(),
            default_model: self.default_model.clone().filter(|m| self.ticked.contains(m)),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Loaded(Result<Option<api::Listed>, String>),
    /// The line under the catalogue: show the row of an agent that takes providers.
    Open,
    Add,
    Edit(String),
    Cancel,
    Kind(Kind),
    Name(String),
    Address(String),
    Key(String),
    ClearKey(bool),
    Test,
    Tested(Result<api::Tested, String>),
    Tick(String, bool),
    ModelInput(String),
    AddModel,
    Default(String),
    Save,
    Saved(Result<(), String>),
    Remove(String),
    Removed(Result<(), String>),
}
fn msg(e: Event) -> Message {
    Message::Agents(agents_ui::Event::Providers(e))
}
fn control<'a>(label: impl text::IntoFragment<'a>, event: Option<Event>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::SMALL))
            .padding([4, 8])
            .style(style::quiet),
        event.map(msg),
    )
}
fn dim<'a>(t: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(t).size(style::SMALL).color(P.fg_dim)
}
fn labelled(label: &str) -> iced::widget::Text<'_> {
    text(label).size(style::SMALL).width(90)
}

impl Providers {
    /// The agent whose row the line under the catalogue opens: the first that takes providers.
    pub fn agent(&self) -> Option<&str> {
        self.agents.first().map(String::as_str).filter(|_| self.supported)
    }

    /// The section for `agent_id`'s expanded row; nothing for an agent that takes no providers.
    pub fn section<'a>(&'a self, agent_id: &str, agent_name: &str, present: bool) -> Option<Element<'a, Message>> {
        if !self.supported || !self.agents.iter().any(|a| a == agent_id) {
            return None;
        }
        let mut section = column![
            text("Model providers").size(style::SMALL),
            dim(format!("OpenRouter, a model server on your network, or any OpenAI-compatible endpoint. Their models appear in {agent_name}'s model picker. Running sessions pick up a change when their jail is restarted.")),
        ]
        .spacing(4);
        if !present {
            section = section.push(dim(format!("Install {agent_name} to use them.")));
        }
        if let Some(note) = &self.note {
            section = section.push(text(note).size(style::SMALL).color(P.error));
        }
        for entry in &self.entries {
            let p = &entry.provider;
            if self.form.as_ref().is_some_and(|f| f.editing.as_deref() == Some(&p.id)) {
                continue;
            }
            let models = match p.models.len() {
                1 => "1 model".to_string(),
                n => format!("{n} models"),
            };
            let armed = self.armed.as_deref() == Some(&p.id);
            section = section.push(
                row![
                    text(&p.name).size(style::TEXT),
                    dim(format!("{} · {models} · {}", entry.endpoint, if entry.has_key { "key saved" } else { "no key" })).width(Fill),
                    control("Edit", (!self.busy).then(|| Event::Edit(p.id.clone()))),
                    control(if armed { "Remove: press again" } else { "Remove" }, (!self.busy).then(|| Event::Remove(p.id.clone()))),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        match &self.form {
            Some(form) => section = section.push(form_view(form)),
            None => {
                if self.entries.is_empty() {
                    section = section.push(dim("No providers"));
                }
                section = section.push(control("Add provider…", Some(Event::Add)));
            }
        }
        Some(container(section).padding([6, 0]).width(Fill).into())
    }

    /// One line under the catalogue for people who come looking.
    pub fn footer<'a>(&'a self, agent_name: &str) -> Element<'a, Message> {
        row![
            dim("Your own model server or OpenRouter?"),
            control(format!("Set up {agent_name} with a model provider"), Some(Event::Open)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }
}

fn form_view(form: &Form) -> Element<'_, Message> {
    let mut body = column![].spacing(6);
    if form.editing.is_none() {
        body = body.push(
            row![labelled("Kind"), pick_list(Kind::ALL, Some(form.kind), |k| msg(Event::Kind(k))).text_size(style::SMALL)]
                .spacing(8)
                .align_y(Alignment::Center),
        );
    }
    body = body.push(
        row![labelled("Name"), text_input("What to call it", &form.name).on_input(|s| msg(Event::Name(s))).padding(7).size(style::SMALL)]
            .spacing(8)
            .align_y(Alignment::Center),
    );
    body = body.push(
        row![labelled("Address"), text_input("https://host/v1", &form.address).on_input(|s| msg(Event::Address(s))).padding(7).size(style::SMALL)]
            .spacing(8)
            .align_y(Alignment::Center),
    );
    body = body.push(dim("The API's root, usually ending in /v1. A server on this machine or your network goes by its network address: localhost inside a session is the session's own jail."));
    if form.kind == Kind::Ollama {
        body = body.push(dim("Ollama: raise the context size on the server (16k–32k to start); with its default, tool calls fail."));
    }
    let key_hint = if form.had_key { "Saved: leave empty to keep it" } else { "Empty for a server that wants none" };
    let mut key = row![
        labelled("API key"),
        text_input(key_hint, &form.key).on_input(|s| msg(Event::Key(s))).secure(true).padding(7).size(style::SMALL),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if form.had_key && form.key.is_empty() {
        key = key.push(checkbox(form.clear_key).label("Forget saved key").text_size(style::SMALL).on_toggle(|v| msg(Event::ClearKey(v))));
    }
    body = body.push(key);
    let mut test = row![control(
        if form.testing { "Testing…" } else { "Test" },
        (!form.testing && !form.address.trim().is_empty()).then_some(Event::Test),
    )]
    .spacing(8)
    .align_y(Alignment::Center);
    test = test.push(match &form.status {
        Some(Ok(said)) => dim(said.as_str()),
        Some(Err(said)) => text(said.as_str()).size(style::SMALL).color(P.error),
        None => dim("Asks the endpoint for its models, from the machine sessions run on."),
    });
    body = body.push(test);

    // models: the ticked ones always, then those the filter keeps, a bounded number of them
    let needle = form.model_input.trim().to_lowercase();
    let mut models = column![text("Models to offer").size(style::SMALL)].spacing(2);
    let (ticked, rest): (Vec<&String>, Vec<&String>) = form.models.iter().partition(|m| form.ticked.contains(*m));
    let matching: Vec<&String> = rest.into_iter().filter(|m| needle.is_empty() || m.to_lowercase().contains(&needle)).collect();
    let hidden = matching.len().saturating_sub(SHOWN_MODELS);
    for model in ticked.into_iter().chain(matching.into_iter().take(SHOWN_MODELS)) {
        models = models.push(checkbox(form.ticked.contains(model)).label(model.as_str()).text_size(style::SMALL).on_toggle({
            let model = model.clone();
            move |v| msg(Event::Tick(model.clone(), v))
        }));
    }
    if hidden > 0 {
        models = models.push(dim(format!("{hidden} more: type below to narrow the list")));
    }
    let typed = form.model_input.trim();
    let can_add = !typed.is_empty() && !form.models.iter().any(|m| m == typed);
    let mut input = text_input("Filter, or a model id the endpoint did not list", &form.model_input)
        .on_input(|s| msg(Event::ModelInput(s)))
        .padding(7)
        .size(style::SMALL);
    if can_add {
        input = input.on_submit(msg(Event::AddModel));
    }
    models = models.push(row![input, control("Add", can_add.then_some(Event::AddModel))].spacing(8).align_y(Alignment::Center));
    body = body.push(models);
    if !form.ticked.is_empty() {
        let mut options = vec![AGENT_CHOOSES.to_string()];
        options.extend(form.ticked.iter().cloned());
        let selected = form.default_model.clone().filter(|m| form.ticked.contains(m)).unwrap_or_else(|| AGENT_CHOOSES.to_string());
        body = body.push(
            row![labelled("Starts with"), pick_list(options, Some(selected), |m| msg(Event::Default(m))).text_size(style::SMALL)]
                .spacing(8)
                .align_y(Alignment::Center),
        );
    }
    body = body.push(dim("Saving allows sessions of agents that use providers to reach this address, and nothing else on its host. The key is given to those agents in their environment."));
    body = body.push(
        row![
            control(if form.saving { "Saving…" } else { "Save" }, form.can_save().then_some(Event::Save)),
            control("Cancel", (!form.saving).then_some(Event::Cancel)),
        ]
        .spacing(10),
    );
    container(body).padding([6, 10]).width(Fill).style(style::fill(P.bg)).into()
}

impl App {
    pub fn providers_load(&self) -> Task<Message> {
        let Some(connect) = self.connect.clone() else {
            return Task::none();
        };
        crate::blocking(
            move || {
                let client = connect().map_err(|e| format!("{e:#}"))?;
                providers::load(&client).map_err(|e| format!("{e:#}"))
            },
            |r| msg(Event::Loaded(r)),
        )
    }

    pub fn providers_event(&mut self, event: Event) -> Task<Message> {
        let connect = self.connect.clone();
        let p = &mut self.agents_page.providers;
        match event {
            Event::Loaded(Ok(listed)) => {
                p.supported = listed.is_some();
                let listed = listed.unwrap_or(api::Listed { providers: vec![], agents: vec![] });
                (p.entries, p.agents, p.note) = (listed.providers, listed.agents, None);
            }
            Event::Loaded(Err(e)) => p.note = Some(e),
            Event::Open => {
                if let Some(agent) = p.agent().map(str::to_string) {
                    return self.agents_reveal(agent);
                }
            }
            Event::Add => (p.form, p.armed) = (Some(Form::new()), None),
            Event::Edit(id) => {
                p.armed = None;
                p.form = p.entries.iter().find(|e| e.provider.id == id).map(Form::of);
            }
            Event::Cancel => p.form = None,
            Event::Remove(id) => {
                if p.armed.as_deref() != Some(&id) {
                    p.armed = Some(id);
                    return Task::none();
                }
                let Some(connect) = connect else { return Task::none() };
                (p.armed, p.busy) = (None, true);
                return crate::blocking(
                    move || {
                        let client = connect().map_err(|e| format!("{e:#}"))?;
                        providers::remove(&client, &id).map_err(|e| format!("{e:#}"))
                    },
                    |r| msg(Event::Removed(r)),
                );
            }
            Event::Removed(result) => {
                p.busy = false;
                p.note = result.err();
                return self.providers_load();
            }
            Event::Saved(result) => {
                match result {
                    Ok(()) => p.form = None,
                    Err(e) => {
                        if let Some(form) = &mut p.form {
                            (form.saving, form.status) = (false, Some(Err(e)));
                        }
                    }
                }
                return self.providers_load();
            }
            event => {
                let taken: Vec<String> = p.entries.iter().map(|e| e.provider.id.clone()).collect();
                let Some(form) = &mut p.form else { return Task::none() };
                match event {
                    Event::Kind(kind) => {
                        // a preset replaces what the previous preset filled in, never what was typed
                        if form.name.trim().is_empty() || form.name == form.kind.name() {
                            form.name = kind.name().into();
                        }
                        if form.address.trim().is_empty() || form.address == form.kind.address() {
                            form.address = kind.address().into();
                        }
                        form.kind = kind;
                    }
                    Event::Name(s) => form.name = s,
                    Event::Address(s) => (form.address, form.status) = (s, None),
                    Event::Key(s) => (form.key, form.clear_key) = (s, false),
                    Event::ClearKey(v) => form.clear_key = v,
                    Event::Tick(model, on) => {
                        if on {
                            form.ticked.insert(model);
                        } else {
                            form.ticked.remove(&model);
                        }
                    }
                    Event::ModelInput(s) => form.model_input = s,
                    Event::AddModel => {
                        let model = std::mem::take(&mut form.model_input).trim().to_string();
                        if !model.is_empty() {
                            form.ticked.insert(model.clone());
                            form.know([model]);
                        }
                    }
                    Event::Default(model) => form.default_model = Some(model).filter(|m| m != AGENT_CHOOSES),
                    Event::Test => {
                        let Some(connect) = connect else { return Task::none() };
                        (form.testing, form.status) = (true, None);
                        let (address, key, id) = (form.address.trim().to_string(), Some(form.key.clone()).filter(|k| !k.is_empty()), form.editing.clone().filter(|_| !form.clear_key));
                        return crate::blocking(
                            move || {
                                let client = connect().map_err(|e| format!("{e:#}"))?;
                                providers::test(&client, &address, key, id).map_err(|e| format!("{e:#}"))
                            },
                            |r| msg(Event::Tested(r)),
                        );
                    }
                    Event::Tested(result) => {
                        form.testing = false;
                        form.status = Some(match result {
                            Ok(tested) => {
                                let said = match tested.models.len() {
                                    0 => format!("It answered with no models; type the ids below. Sessions will reach {}.", tested.endpoint),
                                    1 => format!("1 model. Sessions will reach {}.", tested.endpoint),
                                    n => format!("{n} models. Sessions will reach {}.", tested.endpoint),
                                };
                                form.know(tested.models);
                                Ok(said)
                            }
                            Err(e) => Err(e),
                        });
                    }
                    Event::Save => {
                        let Some(connect) = connect else { return Task::none() };
                        if !form.can_save() {
                            return Task::none();
                        }
                        (form.saving, form.status) = (true, None);
                        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
                        let (provider, key, clear) = (form.provider(&taken), Some(form.key.clone()).filter(|k| !k.is_empty()), form.clear_key);
                        return crate::blocking(
                            move || {
                                let client = connect().map_err(|e| format!("{e:#}"))?;
                                providers::save(&client, provider, key, clear).map(|_| ()).map_err(|e| format!("{e:#}"))
                            },
                            |r| msg(Event::Saved(r)),
                        );
                    }
                    _ => {}
                }
            }
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, key: bool) -> api::Entry {
        api::Entry {
            provider: api::Provider { id: id.into(), name: format!("Name {id}"), api: api::Api::Openai, base_url: "https://x.example/v1".into(), models: vec!["a".into(), "b".into()], default_model: Some("b".into()) },
            has_key: key,
            endpoint: "x.example:443".into(),
        }
    }
    fn listed(agents: &[&str]) -> Event {
        Event::Loaded(Ok(Some(api::Listed { providers: vec![entry("one", true)], agents: agents.iter().map(|a| a.to_string()).collect() })))
    }

    #[test]
    fn the_section_shows_only_for_agents_the_daemon_hands_providers_to() {
        let (mut app, _) = App::new(true, None);
        // a daemon without the capability: nothing anywhere
        let _ = app.providers_event(Event::Loaded(Ok(None)));
        assert!(app.agents_page.providers.section("opencode", "OpenCode", true).is_none());
        assert!(app.agents_page.providers.agent().is_none());
        let _ = app.providers_event(listed(&["opencode"]));
        let p = &app.agents_page.providers;
        assert!(p.section("opencode", "OpenCode", true).is_some());
        assert!(p.section("claude", "Claude Code", true).is_none());
        assert_eq!(p.agent(), Some("opencode"));
    }

    #[test]
    fn a_new_provider_takes_presets_without_overwriting_what_was_typed() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Add);
        let form = |app: &App| app.agents_page.providers.form.as_ref().map(|f| (f.name.clone(), f.address.clone())).unwrap();
        assert_eq!(form(&app), ("OpenRouter".into(), "https://openrouter.ai/api/v1".into()));
        let _ = app.providers_event(Event::Kind(Kind::Ollama));
        assert_eq!(form(&app).0, "Ollama");
        assert!(form(&app).1.ends_with(":11434/v1"));
        let _ = app.providers_event(Event::Address("http://10.0.0.5:11434/v1".into()));
        let _ = app.providers_event(Event::Kind(Kind::LmStudio));
        assert_eq!(form(&app), ("LM Studio".into(), "http://10.0.0.5:11434/v1".into()), "a typed address stays");
    }

    #[test]
    fn the_form_builds_the_record_from_ticked_models() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Add);
        let _ = app.providers_event(Event::Name("One".into()));
        assert!(!app.agents_page.providers.form.as_ref().unwrap().can_save(), "no model ticked");
        let _ = app.providers_event(Event::Tested(Ok(api::Tested { models: vec!["m2".into(), "m1".into()], endpoint: "openrouter.ai:443".into() })));
        let _ = app.providers_event(Event::Tick("m2".into(), true));
        let _ = app.providers_event(Event::ModelInput(" typed/model ".into()));
        let _ = app.providers_event(Event::AddModel);
        let _ = app.providers_event(Event::Default("m2".into()));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert!(form.can_save());
        assert_eq!(form.models, ["m1", "m2", "typed/model"]);
        let record = form.provider(&["one"]);
        assert_eq!(record.id, "one-2", "a new provider never takes an existing id");
        assert_eq!(record.models, ["m2", "typed/model"]);
        assert_eq!(record.default_model.as_deref(), Some("m2"));
        // the default goes with its model
        let _ = app.providers_event(Event::Tick("m2".into(), false));
        assert_eq!(app.agents_page.providers.form.as_ref().unwrap().provider(&[]).default_model, None);
        let _ = app.providers_event(Event::Default(AGENT_CHOOSES.into()));
        assert_eq!(app.agents_page.providers.form.as_ref().unwrap().default_model, None);
    }

    #[test]
    fn editing_keeps_the_id_and_the_saved_key_unless_asked() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Edit("one".into()));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert!(form.had_key && form.key.is_empty() && !form.clear_key);
        assert_eq!(form.provider(&["one"]).id, "one");
        assert_eq!(form.provider(&["one"]).default_model.as_deref(), Some("b"));
        let _ = app.providers_event(Event::ClearKey(true));
        let _ = app.providers_event(Event::Key("sk-new".into()));
        assert!(!app.agents_page.providers.form.as_ref().unwrap().clear_key, "a typed key replaces, it does not forget");
        // a failed save keeps the form and says why
        let _ = app.providers_event(Event::Saved(Err("refused".into())));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert_eq!(form.status, Some(Err("refused".into())));
        let _ = app.providers_event(Event::Saved(Ok(())));
        assert!(app.agents_page.providers.form.is_none());
    }

    #[test]
    fn remove_needs_a_second_press() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Remove("one".into()));
        let p = &app.agents_page.providers;
        assert_eq!(p.armed.as_deref(), Some("one"));
        assert!(!p.busy && p.entries.len() == 1);
    }
}
