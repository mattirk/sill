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
use std::collections::{BTreeMap, BTreeSet};

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
    armed: crate::armed::Armed<String>,
    busy: bool,
    serial: u64,
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

#[derive(Default)]
struct LimitInputs {
    context: String,
    output: String,
}
impl LimitInputs {
    fn value(&self) -> Result<api::ModelLimits, &'static str> {
        let parse = |s: &str| {
            if s.trim().is_empty() {
                return Ok(None);
            }
            s.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or(
                    "Limits must be positive whole token counts, or blank for the agent default.",
                )
        };
        let context = parse(&self.context)?;
        let output = parse(&self.output)?;
        if let (Some(context), Some(output)) = (context, output) {
            if output >= context {
                return Err("Output must be smaller than context.");
            }
        }
        Ok(api::ModelLimits { context, output })
    }
}

struct Form {
    /// The id of the provider being edited; a new one gets its id from its name.
    editing: Option<String>,
    kind: Kind,
    limits_model: Option<String>,
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
    limits: BTreeMap<String, LimitInputs>,
    detecting: Option<(u64, String)>,
    detected: BTreeMap<String, Result<u32, String>>,
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
            limits_model: None,
            name: kind.name().into(),
            address: kind.address().into(),
            key: String::new(),
            had_key: false,
            clear_key: false,
            models: vec![],
            ticked: BTreeSet::new(),
            model_input: String::new(),
            default_model: None,
            limits: BTreeMap::new(),
            detecting: None,
            detected: BTreeMap::new(),
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
            limits: p
                .model_limits
                .iter()
                .map(|(m, l)| {
                    (
                        m.clone(),
                        LimitInputs {
                            context: l.context.map(|n| n.to_string()).unwrap_or_default(),
                            output: l.output.map(|n| n.to_string()).unwrap_or_default(),
                        },
                    )
                })
                .collect(),
            ..Form::new()
        }
    }
    fn know(&mut self, models: impl IntoIterator<Item = String>) {
        self.models.extend(models);
        self.models.sort();
        self.models.dedup();
    }
    fn selected_limits_model(&self) -> Option<&String> {
        self.limits_model.as_ref().filter(|m| self.ticked.contains(*m)).or_else(|| self.ticked.first())
    }
    fn can_save(&self) -> bool {
        !self.saving && self.detecting.is_none()
            && !self.name.trim().is_empty() && !self.address.trim().is_empty() && !self.ticked.is_empty()
            && self
                .ticked
                .iter()
                .all(|m| self.limits.get(m).is_none_or(|l| l.value().is_ok()))
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
            model_limits: self
                .limits
                .iter()
                .filter(|(m, _)| self.ticked.contains(*m))
                .filter_map(|(m, l)| {
                    l.value()
                        .ok()
                        .filter(|l| l.context.is_some() || l.output.is_some())
                        .map(|l| (m.clone(), l))
                })
                .collect(),
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
    LimitsModel(String),
    Context(String, String),
    Output(String, String),
    Detect(String),
    Detected(u64, String, Result<api::DetectedLimits, String>),
    UseDetected(String),
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
            let armed = self.armed.is(&p.id);
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
        body = body.push(dim("Ollama's active context is set on the server. Detect it below, or enter it manually; the model's advertised maximum may be larger than its active allocation."));
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
    if let Some(model) = form.selected_limits_model() {
        models = models.push(row![
            labelled("Limits for"),
            pick_list(form.ticked.iter().cloned().collect::<Vec<_>>(), Some(model.clone()), |m| msg(Event::LimitsModel(m))).text_size(style::SMALL),
        ].spacing(8).align_y(Alignment::Center));
        let empty = String::new();
        let fields = form.limits.get(model);
        let model_context = model.clone();
        let model_output = model.clone();
        let mut limits = column![
            text(format!("{model} · token limits")).size(style::SMALL),
            row![
                text("Context").size(style::SMALL),
                text_input(
                    "Agent default",
                    fields.map(|l| &l.context).unwrap_or(&empty)
                )
                .on_input(move |v| msg(Event::Context(model_context.clone(), v)))
                .size(style::SMALL)
                .padding(7),
                text("Output").size(style::SMALL),
                text_input("Agent default", fields.map(|l| &l.output).unwrap_or(&empty))
                    .on_input(move |v| msg(Event::Output(model_output.clone(), v)))
                    .size(style::SMALL)
                    .padding(7),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(5);
        if let Some(Err(why)) = fields.map(|l| l.value()) {
            limits = limits.push(text(why).size(style::SMALL).color(P.error));
        }
        let detecting = form.detecting.as_ref().is_some_and(|(_, m)| m == model);
        // Offered for every endpoint: nothing in the record says which server answers, and one
        // that is not Ollama answers with the reason.
        limits = limits.push(control(if detecting { "Detecting…" } else { "Detect Ollama context" },
            (!form.saving && form.detecting.is_none() && !form.address.trim().is_empty()).then(|| Event::Detect(model.clone()))));
        if let Some(result) = form.detected.get(model) {
            limits = match result {
                Ok(n) => limits.push(
                    row![
                        dim(format!(
                            "Server is using {n} tokens. Output limit is not reported."
                        )),
                        control(
                            "Use detected context",
                            Some(Event::UseDetected(model.clone()))
                        ),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                ),
                Err(why) => limits.push(text(why).size(style::SMALL).color(P.warning)),
            };
        }
        models = models.push(container(limits).padding([6, 0]));
    }
    if !form.ticked.is_empty() {
        models = models.push(dim("Context is the total token budget; output reserves room for the answer. Blank uses the agent's default. Matching the server's active limit lets the agent compact earlier. Detection reads Ollama only and does not load models or alter the server."));
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
    if let Some(model) = form.ticked.iter().find(|m| form.limits.get(*m).is_some_and(|l| l.value().is_err())) {
        body = body.push(text(format!("Fix token limits for {model} before saving.")).size(style::SMALL).color(P.error));
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
            Event::Add => {
                p.form = Some(Form::new());
                p.armed.clear();
            }
            Event::Edit(id) => {
                p.armed.clear();
                p.form = p.entries.iter().find(|e| e.provider.id == id).map(Form::of);
            }
            Event::Cancel => p.form = None,
            Event::Remove(id) => {
                if !p.armed.is(&id) {
                    p.armed.press(id);
                    return Task::none();
                }
                // with no daemon to ask (the demo) it stays armed
                let Some(connect) = connect else { return Task::none() };
                p.armed.clear();
                p.busy = true;
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
                        form.detecting = None;
                        form.detected.clear();
                    }
                    Event::Name(s) => form.name = s,
                    Event::Address(s) => {
                        (form.address, form.status) = (s, None);
                        form.detecting = None;
                        form.detected.clear();
                    }
                    Event::Key(s) => {
                        (form.key, form.clear_key) = (s, false);
                        form.detecting = None;
                        form.detected.clear();
                    }
                    Event::ClearKey(v) => {
                        form.clear_key = v;
                        form.detecting = None;
                        form.detected.clear();
                    }
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
                    Event::LimitsModel(model) => form.limits_model = Some(model),
                    Event::Context(model, v) => form.limits.entry(model).or_default().context = v,
                    Event::Output(model, v) => form.limits.entry(model).or_default().output = v,
                    Event::UseDetected(model) => {
                        if let Some(Ok(n)) = form.detected.get(&model) {
                            form.limits.entry(model).or_default().context = n.to_string();
                        }
                    }
                    Event::Detect(model) => {
                        if form.detecting.is_some() || form.saving || !form.ticked.contains(&model)
                        {
                            return Task::none();
                        }
                        let Some(connect) = connect else {
                            return Task::none();
                        };
                        p.serial += 1;
                        let serial = p.serial;
                        form.detecting = Some((serial, model.clone()));
                        form.detected.remove(&model);
                        let request = api::DetectLimits {
                            base_url: form.address.trim().into(),
                            model: model.clone(),
                            key: Some(form.key.clone())
                                .filter(|k| !k.is_empty())
                                .map(sill_proto::wire::Secret),
                            provider_id: form.editing.clone().filter(|_| !form.clear_key),
                        };
                        return crate::blocking(
                            move || {
                                let client = connect().map_err(|e| format!("{e:#}"))?;
                                providers::detect_limits(&client, request)
                                    .map_err(|e| format!("{e:#}"))
                            },
                            move |r| msg(Event::Detected(serial, model.clone(), r)),
                        );
                    }
                    Event::Detected(serial, model, result) => {
                        if form.detecting.as_ref() != Some(&(serial, model.clone())) {
                            return Task::none();
                        }
                        form.detecting = None;
                        form.detected.insert(model, result.map(|r| r.context));
                    }
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
            provider: api::Provider { id: id.into(), name: format!("Name {id}"), api: api::Api::Openai, base_url: "https://x.example/v1".into(), models: vec!["a".into(), "b".into()], default_model: Some("b".into()),
                model_limits: BTreeMap::new(),
            },
            has_key: key,
            endpoint: "x.example:443".into(),
        }
    }
    fn listed(agents: &[&str]) -> Event {
        Event::Loaded(Ok(Some(api::Listed { providers: vec![entry("one", true)], agents: agents.iter().map(|a| a.to_string()).collect() })))
    }

    #[test]
    fn the_limit_editor_selects_one_model() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Edit("one".into()));
        let _ = app.providers_event(Event::Context("a".into(), "32768".into()));
        let _ = app.providers_event(Event::LimitsModel("b".into()));
        let _ = app.providers_event(Event::Context("b".into(), "16384".into()));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert_eq!(form.selected_limits_model().map(String::as_str), Some("b"));
        assert_eq!(form.provider(&[]).model_limits["a"].context, Some(32768));
        assert_eq!(form.provider(&[]).model_limits["b"].context, Some(16384));
        let _ = app.providers_event(Event::Tick("b".into(), false));
        assert_eq!(app.agents_page.providers.form.as_ref().unwrap().selected_limits_model().map(String::as_str), Some("a"));
    }

    #[test]
    #[ignore = "writes provider token limits PNG for review"]
    fn render_model_limits() {
        let mut form = Form::new();
        form.editing = Some("lan-ollama".into());
        form.kind = Kind::Ollama;
        form.name = "Ollama (workstation)".into();
        form.address = "http://192.168.1.20:11434/v1".into();
        let model = "qwen3-coder:30b".to_string();
        form.know([model.clone()]);
        form.ticked.insert(model.clone());
        form.limits.insert(
            model.clone(),
            LimitInputs {
                context: "32768".into(),
                output: "4096".into(),
            },
        );
        form.detected.insert(model, Ok(32768));
        let mut ui = iced_test::Simulator::with_size(
            crate::snapshots::settings(),
            (680.0, 1000.0),
            form_view(&form),
        );
        let image = ui.snapshot(&crate::style::theme()).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sill-gui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("provider-limits-")
            {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        assert!(image.matches_image(dir.join("provider-limits")).unwrap());
    }

    #[test]
    fn model_limits_are_validated_preserved_and_omitted_when_unticked() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Edit("one".into()));
        let _ = app.providers_event(Event::Context("a".into(), "32768".into()));
        let _ = app.providers_event(Event::Output("a".into(), "4096".into()));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert!(form.can_save());
        let record = form.provider(&[]);
        assert_eq!(
            record.model_limits["a"],
            api::ModelLimits {
                context: Some(32768),
                output: Some(4096)
            }
        );
        let form = Form::of(&api::Entry {
            provider: record,
            has_key: false,
            endpoint: "x.example:443".into(),
        });
        assert_eq!(form.limits["a"].context, "32768");
        let _ = app.providers_event(Event::Output("a".into(), "32768".into()));
        assert!(!app.agents_page.providers.form.as_ref().unwrap().can_save());
        let _ = app.providers_event(Event::Tick("a".into(), false));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert!(form.can_save());
        assert!(form.provider(&[]).model_limits.is_empty());
        assert!(
            LimitInputs {
                context: "invalid".into(),
                output: String::new()
            }
            .value()
            .is_err()
        );
        assert!(
            LimitInputs {
                context: "0".into(),
                output: String::new()
            }
            .value()
            .is_err()
        );
    }

    #[test]
    fn detection_is_a_suggestion_and_stale_results_are_ignored() {
        let (mut app, _) = App::new(true, None);
        let _ = app.providers_event(listed(&["opencode"]));
        let _ = app.providers_event(Event::Edit("one".into()));
        let _ = app.providers_event(Event::Context("a".into(), "16384".into()));
        app.agents_page.providers.form.as_mut().unwrap().detecting = Some((1, "a".into()));
        let _ = app.providers_event(Event::Detected(
            1,
            "a".into(),
            Ok(api::DetectedLimits { context: 32768 }),
        ));
        assert_eq!(
            app.agents_page.providers.form.as_ref().unwrap().limits["a"].context,
            "16384"
        );
        let _ = app.providers_event(Event::UseDetected("a".into()));
        assert_eq!(
            app.agents_page.providers.form.as_ref().unwrap().limits["a"].context,
            "32768"
        );
        app.agents_page.providers.form.as_mut().unwrap().detecting = Some((2, "a".into()));
        let _ = app.providers_event(Event::Address("http://new.example/v1".into()));
        let _ = app.providers_event(Event::Detected(
            2,
            "a".into(),
            Ok(api::DetectedLimits { context: 65536 }),
        ));
        let form = app.agents_page.providers.form.as_ref().unwrap();
        assert!(form.detected.is_empty());
        assert_eq!(form.limits["a"].context, "32768");
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
        assert!(p.armed.is(&"one".to_string()));
        assert!(!p.busy && p.entries.len() == 1);
    }
}
