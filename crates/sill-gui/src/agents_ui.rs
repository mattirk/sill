//! Registry browsing and agent administration. Jobs outlive navigation away from the page.
use crate::{
    App, Message, keys,
    style::{self, P},
};
use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, toggler,
};
use iced::{Alignment, Element, Fill, Task};
use sill_client::agents::{self, Entry};
use sill_proto::agent_admin::Definition;
use sill_proto::host::RegistryEntry;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct AgentsPage {
    pub shown: bool,
    entries: Vec<Entry>,
    search: String,
    filter: Filter,
    untested: bool,
    loading: bool,
    reload_after: bool,
    pub auto_fetch_attempted: bool,
    icons: BTreeMap<String, iced::widget::svg::Handle>,
    icons_queued: BTreeSet<String>,
    busy: BTreeSet<String>,
    logs: BTreeMap<String, String>,
    note: Option<String>,
    armed: Option<String>,
    editor: Option<Editor>,
    details: Option<String>,
    /// Directory proposals the person unticked, per agent, until Apply.
    unticked: BTreeMap<String, BTreeSet<String>>,
}
struct Editor {
    id: String,
    definition: Definition,
    command: String,
    env: iced::widget::text_editor::Content,
    domain_input: String,
    domain_error: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Installed,
    Absent,
}
impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::All => "All",
            Self::Installed => "Installed",
            Self::Absent => "Not installed",
        })
    }
}
#[derive(Debug, Clone)]
pub enum Event {
    Load(bool),
    Loaded(Result<agents::Catalog, String>),
    Search(String),
    Filter(Filter),
    Untested(bool),
    /// Install or update to the entry's target version (see [`target`]).
    Install(String),
    /// The per-agent version choice: on = Latest, off = Supported.
    Latest(String, bool),
    Remove(String),
    Reset(String),
    Edit(String),
    CloseEditor,
    Command(String),
    Env(iced::widget::text_editor::Action),
    DomainInput(String),
    AddDomain,
    RemoveDomain(String),
    Enabled(bool),
    Save,
    Icon(String, Option<Vec<u8>>),
    Log(String, String),
    Done(String, Result<(), String>),
    /// Find the agent's directories: a first start in a throwaway jail, or (with a method) a
    /// login on the daemon host whose files are kept.
    Discover(String, Option<String>),
    /// Tick or untick one proposed path.
    Choose(String, String, bool),
    /// Save the ticked proposals as the agent's `home_paths`.
    ApplySetup(String),
}
fn msg(e: Event) -> Message {
    Message::Agents(e)
}
/// The version Install and Update act on: with **Latest** chosen, the live index's newer version
/// when there is one; otherwise, and with **Supported**, the snapshot's (or the live entry itself
/// when the snapshot does not know it).
fn target<'a>(settings: &crate::settings::Settings, e: &'a Entry) -> &'a RegistryEntry {
    let r = &e.registry;
    match &r.newer {
        Some(newer) if settings.prefers_latest(&r.entry_id) => newer,
        _ => r,
    }
}
fn control(label: &str, event: Event) -> Element<'_, Message> {
    keys::press(
        button(text(label).size(style::SMALL))
            .padding([4, 8])
            .style(style::quiet),
        msg(event),
    )
}
fn domain_controls(editor: &Editor) -> Element<'_, Message> {
    let mut domains = column![
        text("Allowed domains").size(style::SMALL),
        text("Save applies to new sessions. Running sessions keep their access; policies may allow other domains.")
            .size(style::SMALL).color(P.fg_dim),
    ].spacing(4);
    for domain in &editor.definition.domains {
        domains = domains.push(
            row![
                text(domain).size(style::TEXT).width(Fill),
                keys::press(
                    button(crate::icons::glyph("minus", 13.0, style::alpha(P.fg, 0.7)))
                        .padding([6, 8])
                        .style(style::quiet),
                    msg(Event::RemoveDomain(domain.clone())),
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    if editor.definition.domains.is_empty() {
        domains = domains.push(text("No domains added").size(style::SMALL).color(P.fg_dim));
    }
    let pattern = agents::domain_pattern(&editor.domain_input);
    let duplicate = pattern.as_ref().is_some_and(|domain| {
        editor
            .definition
            .domains
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain))
    });
    let can_add = pattern.is_some() && !duplicate;
    let mut input = text_input("example.com or *.example.com", &editor.domain_input)
        .on_input(|s| msg(Event::DomainInput(s)))
        .padding(7)
        .size(style::SMALL);
    if can_add {
        input = input.on_submit(msg(Event::AddDomain));
    }
    domains = domains.push(
        row![
            input,
            keys::press_maybe(
                button(text("Add").size(style::SMALL))
                    .padding([6, 10])
                    .style(style::quiet),
                can_add.then(|| msg(Event::AddDomain))
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    if let Some(error) = &editor.domain_error {
        domains = domains.push(text(error).size(style::SMALL).color(P.error));
    } else if !editor.domain_input.trim().is_empty() && !can_add {
        domains = domains.push(
            text(if duplicate {
                "Already in the list"
            } else {
                "Use a domain, optionally with :port; leave out https:// and paths"
            })
            .size(style::SMALL)
            .color(P.fg_dim),
        );
    }
    domains.into()
}

/// Where an agent without `home_paths` keeps its state. The login is what has to persist, and
/// most agents write nothing before one: the card offers the agent's login methods; the chosen
/// login runs on this machine with an empty home, what it wrote is listed with a tick each, and
/// Apply declares it (the files are kept already). A bare first start is the fallback.
fn setup_card<'a>(e: &'a Entry, unticked: Option<&'a BTreeSet<String>>) -> Element<'a, Message> {
    let id = &e.registry.entry_id;
    let mut card = column![].spacing(4);
    let dim = |t: &'a str| text(t).size(style::SMALL).color(P.fg_dim);
    match &e.discovery {
        None => {
            card = card.push(dim("Sill does not know yet where this agent keeps its login and settings. Find out by starting it once in a throwaway jail with an empty home; nothing is saved until you apply."));
            card = card.push(control("Find directories", Event::Discover(id.clone(), None)));
        }
        Some(discovery) => {
            let logged_in = discovery.login_method.is_some();
            if logged_in {
                let method = discovery.login_method.as_deref().unwrap_or_default();
                let name = discovery.auth_methods.iter().find(|m| m.id == method).map(|m| m.name.as_str()).unwrap_or(method);
                card = card.push(text(format!("Logged in with {name}. It keeps its login and settings here; ticked paths become this agent's own, kept across sessions in sill's storage.")).size(style::SMALL).color(P.fg_dim));
            } else if let Some(note) = &discovery.note {
                card = card.push(text(format!("The probe could not finish: {note}. Nothing to go on yet; probe again, or log in below if it offered a way.")).size(style::SMALL).color(P.warning));
            } else if discovery.proposals.is_empty() {
                card = card.push(dim("On its first start it created nothing under the home: it writes its state when it logs in. Log in once here, on this machine, and Sill keeps what the login writes."));
            } else {
                card = card.push(dim("On its first start it created these; ticked paths become this agent's own, kept across sessions in sill's storage. A login may add more: log in here to see."));
            }
            for proposal in &discovery.proposals {
                let ticked = !unticked.is_some_and(|u| u.contains(&proposal.path));
                let label = match proposal.kind {
                    sill_proto::agent_admin::HomePathKind::Directory => format!("{}/", proposal.path),
                    sill_proto::agent_admin::HomePathKind::File => proposal.path.clone(),
                };
                let mut line = row![
                    iced::widget::checkbox(ticked)
                        .label(label)
                        .text_size(style::SMALL)
                        .on_toggle({
                            let id = id.clone();
                            let path = proposal.path.clone();
                            move |v| msg(Event::Choose(id.clone(), path.clone(), v))
                        }),
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                if proposal.on_host {
                    line = line.push(
                        text("exists on this machine: owning it hides the host's copy from this agent")
                            .size(style::TINY)
                            .color(P.warning),
                    );
                }
                card = card.push(line);
                let root = format!("{}/", proposal.path.trim_start_matches("~/"));
                for file in discovery.login_files.iter().filter(|f| f.starts_with(&root)) {
                    card = card.push(container(text(format!("login file: ~/{file}")).size(style::TINY).color(P.fg_dim)).padding([0, 28]));
                }
            }
            let mut actions = row![].spacing(10);
            if !discovery.proposals.is_empty() {
                let chosen = discovery.proposals.iter().filter(|p| !unticked.is_some_and(|u| u.contains(&p.path))).count();
                actions = actions.push(control(if chosen == 0 { "Apply: no persistent state" } else { "Apply" }, Event::ApplySetup(id.clone())));
            }
            if discovery.auth_methods.is_empty() && discovery.note.is_none() && !logged_in {
                card = card.push(dim("It offered no login method at its first start, so there is nothing to log in with here."));
            }
            if !discovery.auth_methods.is_empty() {
                actions = actions.push(dim(if logged_in { "Log in again:" } else { "Log in with:" }));
                for m in &discovery.auth_methods {
                    actions = actions.push(control(&m.name, Event::Discover(id.clone(), Some(m.id.clone()))));
                }
            }
            actions = actions.push(control("Probe again", Event::Discover(id.clone(), None)));
            card = card.push(actions.wrap());
        }
    }
    container(card).padding([4, 0]).width(Fill).into()
}

impl App {
    pub fn agents_load(&mut self, refresh: bool) -> Task<Message> {
        if self.agents_page.loading {
            self.agents_page.reload_after = true;
            return Task::none();
        }
        let Some(connect) = self.connect.clone() else {
            self.agents_page.note =
                Some("The demo has no daemon; agent management needs a connection.".into());
            return Task::none();
        };
        self.agents_page.loading = true;
        if refresh {
            self.agents_page.auto_fetch_attempted = true;
        }
        crate::blocking(
            move || {
                let client = connect().map_err(|e| format!("{e:#}"))?;
                if refresh {
                    agents::refresh(&client).map_err(|e| format!("{e:#}"))?;
                }
                agents::load(&client).map_err(|e| format!("{e:#}"))
            },
            |r| msg(Event::Loaded(r)),
        )
    }
    fn agents_icons(&mut self) -> Task<Message> {
        let p = &mut self.agents_page;
        let selectors: Vec<_> = p
            .entries
            .iter()
            .filter(|e| e.registry.has_icon && !p.icons_queued.contains(&e.registry.entry_id))
            .map(|e| e.selector())
            .collect();
        if selectors.is_empty() {
            return Task::none();
        }
        let Some(connect) = self.connect.clone() else {
            return Task::none();
        };
        for s in &selectors {
            p.icons_queued.insert(s.entry_id.clone());
        }
        let (tx, rx) = async_channel::bounded(32);
        std::thread::spawn(move || {
            let Ok(client) = connect() else {
                return;
            };
            for selector in selectors {
                let id = selector.entry_id.clone();
                let bytes = agents::icon(&client, selector).ok().flatten();
                if tx.send_blocking(Event::Icon(id, bytes)).is_err() {
                    break;
                }
            }
        });
        Task::run(
            iced::futures::stream::unfold(
                rx,
                |rx| async move { rx.recv().await.ok().map(|e| (e, rx)) },
            ),
            msg,
        )
    }
    pub fn agents_event(&mut self, event: Event) -> Task<Message> {
        let p = &mut self.agents_page;
        match event {
            Event::Load(refresh) => return self.agents_load(refresh),
            Event::Loaded(result) => {
                p.loading = false;
                match result {
                    Ok(catalog) => {
                        let refresh_due = catalog.refresh_due && !p.auto_fetch_attempted;
                        p.entries = catalog.entries;
                        p.note = None;
                        let icons = self.agents_icons();
                        if refresh_due {
                            return Task::batch([icons, self.agents_load(true)]);
                        }
                        if std::mem::take(&mut self.agents_page.reload_after) {
                            return Task::batch([icons, self.agents_load(false)]);
                        }
                        return icons;
                    }
                    Err(e) => p.note = Some(e),
                }
                if std::mem::take(&mut p.reload_after) {
                    return self.agents_load(false);
                }
            }
            Event::Search(s) => p.search = s,
            Event::Filter(f) => p.filter = f,
            Event::Untested(v) => p.untested = v,
            Event::Icon(id, bytes) => {
                if let Some(bytes) = bytes {
                    p.icons
                        .insert(id, iced::widget::svg::Handle::from_memory(bytes));
                }
            }
            Event::Log(id, line) => {
                p.logs.insert(id, line.chars().take(16384).collect());
            }
            Event::Done(id, result) => {
                p.busy.remove(&id);
                if result.is_ok() {
                    p.unticked.remove(&id);
                }
                p.logs
                    .insert(id, result.err().unwrap_or_else(|| "Done".into()));
                return self.agents_load(false);
            }
            Event::CloseEditor => {
                p.editor = None;
                p.details = None;
            }
            Event::Edit(id) => {
                p.editor = None;
                if p.details.as_deref() == Some(&id) {
                    p.details = None;
                    return Task::none();
                }
                p.details = Some(id.clone());
                if let Some(d) = p
                    .entries
                    .iter()
                    .find(|e| e.registry.entry_id == id)
                    .and_then(|e| e.definition.clone())
                {
                    p.editor = Some(Editor {
                        id,
                        command: serde_json::to_string(&d.command).unwrap(),
                        env: iced::widget::text_editor::Content::with_text(
                            &d.env
                                .iter()
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        domain_input: String::new(),
                        domain_error: None,
                        definition: d,
                    });
                }
            }
            Event::Command(s) => {
                if let Some(e) = &mut p.editor {
                    e.command = s;
                }
            }
            Event::Env(s) => {
                if let Some(e) = &mut p.editor {
                    e.env.perform(s);
                }
            }
            Event::DomainInput(s) => {
                if let Some(e) = &mut p.editor {
                    e.domain_input = s;
                    e.domain_error = None;
                }
            }
            Event::AddDomain => {
                if let Some(e) = &mut p.editor {
                    let Some(domain) = agents::domain_pattern(&e.domain_input) else {
                        e.domain_error = Some("Use a domain such as example.com or *.example.com, without https:// or a path".into());
                        return Task::none();
                    };
                    if e.definition
                        .domains
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(&domain))
                    {
                        e.domain_error = Some("Already in the list".into());
                        return Task::none();
                    }
                    e.definition.domains.push(domain);
                    e.domain_input.clear();
                    e.domain_error = None;
                }
            }
            Event::RemoveDomain(domain) => {
                if let Some(e) = &mut p.editor {
                    e.definition.domains.retain(|d| d != &domain);
                }
            }
            Event::Enabled(v) => {
                if let Some(e) = &mut p.editor {
                    e.definition.enabled = v;
                }
            }
            Event::Save => {
                let Some(editor) = &p.editor else {
                    return Task::none();
                };
                let mut definition = editor.definition.clone();
                definition.command = match serde_json::from_str::<Vec<String>>(&editor.command) {
                    Ok(v) if !v.is_empty() && !v[0].is_empty() => v,
                    _ => {
                        p.note = Some("Command must be a JSON list of arguments, starting with the executable.".into());
                        return Task::none();
                    }
                };
                let mut env = BTreeMap::new();
                for line in editor.env.text().lines().filter(|s| !s.trim().is_empty()) {
                    let Some((k, v)) = line.split_once('=').filter(|(k, _)| !k.trim().is_empty())
                    else {
                        p.note = Some("Environment needs one NAME=value per line.".into());
                        return Task::none();
                    };
                    env.insert(k.trim().into(), v.into());
                }
                definition.env = env;
                if !editor.domain_input.trim().is_empty() {
                    p.note = Some("Add the typed domain or clear it before saving.".into());
                    return Task::none();
                }
                let id = editor.id.clone();
                return self.agent_call(
                    id,
                    "_sill/agents.save",
                    serde_json::to_value(definition).unwrap(),
                );
            }
            Event::Latest(id, latest) => {
                if latest {
                    self.settings.supported_agents.remove(&id);
                } else {
                    self.settings.supported_agents.insert(id);
                }
                self.save_settings();
            }
            Event::Choose(id, path, ticked) => {
                let set = p.unticked.entry(id).or_default();
                if ticked {
                    set.remove(&path);
                } else {
                    set.insert(path);
                }
            }
            Event::ApplySetup(id) => {
                let Some(entry) = p.entries.iter().find(|e| e.registry.entry_id == id) else {
                    return Task::none();
                };
                let (Some(definition), Some(discovery)) = (&entry.definition, &entry.discovery) else {
                    return Task::none();
                };
                let unticked = p.unticked.get(&id).cloned().unwrap_or_default();
                let definition = agents::set_up(definition, discovery, &unticked);
                return self.agent_call(
                    id,
                    "_sill/agents.save",
                    serde_json::to_value(definition).unwrap(),
                );
            }
            Event::Discover(id, method) => {
                if p.busy.contains(&id) {
                    return Task::none();
                }
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                p.busy.insert(id.clone());
                p.unticked.remove(&id);
                p.logs.insert(id.clone(), if method.is_some() { "Logging in on this machine…".into() } else { "Starting it once in a throwaway jail…".into() });
                let (tx, rx) = async_channel::bounded(64);
                std::thread::spawn(move || {
                    let result = connect()
                        .and_then(|client| {
                            agents::discover(&client, &id, method.as_deref(), |line| {
                                let _ = tx.send_blocking(Event::Log(id.clone(), line));
                            })
                        })
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send_blocking(Event::Done(id, result));
                });
                let stream = iced::futures::stream::unfold(rx, |rx| async move {
                    rx.recv().await.ok().map(|e| (e, rx))
                });
                return Task::run(stream, msg);
            }
            Event::Install(id) => {
                if p.busy.contains(&id) {
                    return Task::none();
                }
                let Some(entry) = p.entries.iter().find(|e| e.registry.entry_id == id) else {
                    return Task::none();
                };
                let entry = target(&self.settings, entry).clone();
                let key = format!("install:{id}:{}:{}", entry.version, entry.source);
                if entry.source == "live" && p.armed.as_deref() != Some(&key) {
                    p.armed = Some(key);
                    p.logs.insert(id, format!("Unreviewed live index: {} v{}; {}. Press the same action again to confirm.", entry.name, entry.version, entry.install_spec));
                    return Task::none();
                }
                p.armed = None;
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                p.busy.insert(id.clone());
                p.logs.insert(id.clone(), "Installing…".into());
                let (tx, rx) = async_channel::bounded(64);
                std::thread::spawn(move || {
                    let result = connect()
                        .and_then(|client| {
                            agents::install(&client, &entry, |line| {
                                let _ = tx.send_blocking(Event::Log(id.clone(), line));
                            })
                        })
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send_blocking(Event::Done(id, result));
                });
                let stream = iced::futures::stream::unfold(rx, |rx| async move {
                    rx.recv().await.ok().map(|e| (e, rx))
                });
                return Task::run(stream, msg);
            }
            Event::Remove(ref id) | Event::Reset(ref id) => {
                let reset = matches!(event, Event::Reset(_));
                let id = id.clone();
                if p.busy.contains(&id) {
                    return Task::none();
                }
                let key = format!("{}:{id}", if reset { "reset" } else { "remove" });
                if p.armed.as_deref() != Some(&key) {
                    p.armed = Some(key);
                    p.logs.insert(id, if reset {"Reset stops running sessions and deletes this agent's Sill login, settings, history and saved API keys. Press Reset again to confirm."} else {"Remove the Sill-managed installation. Press Remove again to confirm."}.into());
                    return Task::none();
                }
                p.armed = None;
                let Some(entry) = p.entries.iter().find(|e| e.registry.entry_id == id) else {
                    return Task::none();
                };
                let (method, params) = if reset {
                    let Some(d) = &entry.definition else {
                        return Task::none();
                    };
                    ("_sill/agents.reset", serde_json::json!({"agent_id":d.id}))
                } else {
                    let mut selector = entry.selector();
                    if let Some(i) = &entry.installed {
                        selector.version = i.version.clone();
                        selector.source = i.source.clone();
                    }
                    (
                        "_sill/registry.uninstall",
                        serde_json::to_value(selector).unwrap(),
                    )
                };
                return self.agent_call(id, method, params);
            }
        }
        Task::none()
    }
    fn agent_call(
        &mut self,
        id: String,
        method: &'static str,
        params: serde_json::Value,
    ) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        if !self.agents_page.busy.insert(id.clone()) {
            return Task::none();
        }
        crate::blocking(
            move || {
                let outcome = actions.call(method, params);
                if outcome.is_done() {
                    Ok(())
                } else {
                    Err(outcome
                        .problem()
                        .unwrap_or("Unknown outcome; refresh before retrying")
                        .to_string())
                }
            },
            move |result| msg(Event::Done(id.clone(), result)),
        )
    }
    pub fn agents_view(&self) -> Element<'_, Message> {
        let p = &self.agents_page;
        let mut body = column![
            row![
                text_input("Search agents…", &p.search)
                    .on_input(|s| msg(Event::Search(s)))
                    .padding(8),
                pick_list(
                    [Filter::All, Filter::Installed, Filter::Absent],
                    Some(p.filter),
                    |f| msg(Event::Filter(f))
                ),
                toggler(p.untested)
                    .label("Untested")
                    .on_toggle(|v| msg(Event::Untested(v))),
                control("Refresh", Event::Load(true)),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .wrap()
        ]
        .spacing(4);
        if p.loading {
            body = body.push(text("Loading agents…").color(P.fg_dim));
        }
        if let Some(note) = &p.note {
            body = body.push(text(note).color(P.error));
            body = body.push(control("Try again", Event::Load(false)));
        }
        let needle = p.search.to_lowercase();
        let mut count = 0;
        for e in &p.entries {
            if !p.untested && !e.present() && !e.tested() {
                continue;
            }
            if (p.filter == Filter::Installed && !e.present())
                || (p.filter == Filter::Absent && e.present())
            {
                continue;
            }
            let r = &e.registry;
            if !format!("{} {} {}", r.name, r.entry_id, r.description)
                .to_lowercase()
                .contains(&needle)
            {
                continue;
            }
            count += 1;
            let id = &r.entry_id;
            let latest = self.settings.prefers_latest(id);
            let wanted = target(&self.settings, e);
            let mut top = row![
                match p.icons.get(id) {
                    Some(handle) => iced::widget::svg(handle.clone())
                        .width(16)
                        .height(16)
                        .style(style::icon(P.fg))
                        .into(),
                    None => crate::icons::tab_agent(
                        r.builtin_agent_id.as_deref().unwrap_or(id),
                        P.fg,
                        false
                    ),
                },
                text(&r.name).size(style::TEXT).width(Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if p.busy.contains(id) {
                top = top.push(text("Working…").size(style::SMALL).color(P.accent));
            } else if e.needs_setup() {
                top = top.push(text("Needs setup").size(style::SMALL).color(P.warning));
            } else if let Some(installed) = &e.installed {
                if installed.version != wanted.version {
                    top = top.push(control("Update", Event::Install(id.clone())));
                } else {
                    top = top.push(text("Installed").size(style::SMALL).color(P.fg_dim));
                }
            } else if e.available {
                top = top.push(text("On this machine").size(style::SMALL).color(P.fg_dim));
            } else if wanted.installable {
                top = top.push(control("Install", Event::Install(id.clone())));
            }
            let expanded = p.details.as_deref() == Some(id);
            top = top.push(control(
                if expanded {
                    "Settings ▴"
                } else {
                    "Settings ▾"
                },
                Event::Edit(id.clone()),
            ));
            let mut card = column![top].spacing(6);
            if e.needs_setup() && !p.busy.contains(id) {
                card = card.push(setup_card(e, p.unticked.get(id)));
            }
            if expanded {
                let mut versions = format!("{} · supported v{} ({})", id, r.version, r.source);
                if let Some(newer) = &r.newer {
                    versions.push_str(&format!(" · latest v{} (live, unreviewed)", newer.version));
                }
                card = card.push(text(versions).size(style::SMALL).color(P.fg_dim));
                card = card.push(text(&r.description).size(style::SMALL).color(P.fg_dim));
                if !r.installable && !e.present() {
                    card = card.push(
                        text("No build for this host")
                            .size(style::SMALL)
                            .color(P.fg_dim),
                    );
                }
                if let Some(installed) = &e.installed {
                    card = card.push(
                        text(format!(
                            "Installed {} · {}",
                            installed.version, installed.source
                        ))
                        .size(style::SMALL)
                        .color(P.fg_dim),
                    );
                }
                let mut links = row![].spacing(8).align_y(Alignment::Center);
                for (label, url) in [
                    ("Copy source link", &r.repository),
                    ("Copy website link", &r.website),
                ] {
                    if let Some(url) = url {
                        links = links.push(keys::press(
                            button(text(label).size(style::SMALL)).style(style::quiet),
                            Message::Copy(url.clone()),
                        ));
                    }
                }
                if e.installed.is_some() {
                    links = links.push(control("Remove", Event::Remove(id.clone())));
                }
                if e.definition.is_some() {
                    links = links.push(control("Reset", Event::Reset(id.clone())));
                }
                card = card.push(links.wrap());
                card = card.push(
                    toggler(latest)
                        .label(if latest {
                            "Version: Latest — the live index's newest, unreviewed"
                        } else {
                            "Version: Supported — sill's reviewed snapshot"
                        })
                        .text_size(style::SMALL)
                        .on_toggle({
                            let id = id.clone();
                            move |v| msg(Event::Latest(id.clone(), v))
                        }),
                );
            }
            if let Some(log) = p
                .logs
                .get(id)
                .filter(|log| expanded || log.as_str() != "Done")
            {
                card = card.push(crate::selection::text(log.clone()).size(style::SMALL));
            }
            if let Some(editor) = p.editor.as_ref().filter(|editor| &editor.id == id) {
                card = card.push(column![
                    text("Command (JSON argument list)").size(style::SMALL),
                    text_input("[\"executable\", \"argument\"]", &editor.command)
                        .on_input(|s| msg(Event::Command(s)))
                        .padding(7),
                    text("Environment (NAME=value per line; may contain API keys)")
                        .size(style::SMALL),
                    iced::widget::text_editor(&editor.env)
                        .on_action(|a| msg(Event::Env(a)))
                        .height(100)
                        .padding(7),
                    domain_controls(editor),
                    toggler(editor.definition.enabled)
                        .label("Offer in the new session menu")
                        .on_toggle(|v| msg(Event::Enabled(v))),
                    row![
                        control("Save", Event::Save),
                        control("Close", Event::CloseEditor)
                    ]
                    .spacing(10),
                ]);
            }
            body = body.push(
                container(card)
                    .padding([6, 10])
                    .width(Fill)
                    .style(style::fill(P.surface)),
            );
        }
        if count == 0 && !p.loading {
            body = body.push(text("No matching agents").color(P.fg_dim));
        }
        scrollable(container(body).max_width(960).padding(16).center_x(Fill))
            .height(Fill)
            .style(style::feed_scroll)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowed_domains_use_the_agent_save_and_keep_failed_edits() {
        use crate::tests::{connector, daemon, settle_task};
        use serde_json::json;
        let (mut app, _) = App::new(true, None);
        let definition: Definition = serde_json::from_value(json!({
            "id":"codex", "name":"Codex", "command":["codex-acp"],
            "env":{"KEEP":"yes"}, "domains":["openai.com", "google.com"]
        }))
        .unwrap();
        app.agents_page.editor = Some(Editor {
            id: "codex-acp".into(),
            command: "[\"codex-acp\"]".into(),
            env: iced::widget::text_editor::Content::with_text("KEEP=yes"),
            domain_input: String::new(),
            domain_error: None,
            definition,
        });
        let _ = app.update(msg(Event::DomainInput("https://example.com/".into())));
        let _ = app.update(msg(Event::AddDomain));
        assert!(
            app.agents_page
                .editor
                .as_ref()
                .unwrap()
                .domain_error
                .is_some()
        );
        let _ = app.update(msg(Event::DomainInput("GOOGLE.com".into())));
        let _ = app.update(msg(Event::AddDomain));
        assert_eq!(
            app.agents_page
                .editor
                .as_ref()
                .unwrap()
                .definition
                .domains
                .len(),
            2
        );
        let _ = app.update(msg(Event::RemoveDomain("google.com".into())));
        let _ = app.update(msg(Event::DomainInput("  *.Example.com:8443  ".into())));
        let _ = app.update(msg(Event::AddDomain));
        let editor = app.agents_page.editor.as_ref().unwrap();
        assert_eq!(
            editor.definition.domains,
            ["openai.com", "*.example.com:8443"]
        );
        assert!(editor.domain_input.is_empty());

        let (client, mut daemon) = daemon();
        app.actions = Some(sill_client::actions::Actions::new(connector(vec![client])));
        let worker = std::thread::spawn(move || {
            let req = daemon.expect("_sill/agents.save");
            assert_eq!(req["params"]["id"], "codex");
            assert_eq!(
                req["params"]["domains"],
                json!(["openai.com", "*.example.com:8443"])
            );
            assert_eq!(req["params"]["env"], json!({"KEEP":"yes"}));
            assert_eq!(req["params"]["command"], json!(["codex-acp"]));
            daemon.refuse(&req, "permission_denied", "Cannot save");
        });
        // A typed domain must be added or cleared before Save can send anything.
        let _ = app.update(msg(Event::DomainInput("pending.com".into())));
        let _ = app.update(msg(Event::Save));
        assert!(
            app.agents_page
                .note
                .as_ref()
                .unwrap()
                .contains("Add the typed domain")
        );
        assert!(app.agents_page.busy.is_empty());
        let _ = app.update(msg(Event::DomainInput(String::new())));
        let task = app.update(msg(Event::Save));
        settle_task(&mut app, task);
        worker.join().unwrap();
        assert!(app.agents_page.logs["codex-acp"].contains("Cannot save"));
        assert_eq!(
            app.agents_page.editor.as_ref().unwrap().definition.domains,
            ["openai.com", "*.example.com:8443"]
        );
    }

    #[test]
    fn agents_navigation_returns_to_the_conversation_and_keeps_job_state() {
        let (mut app, _) = App::new(true, None);
        let _ = app.update(Message::ShowPage(crate::Page::Agents));
        assert!(app.backend_shown());
        assert!(app.agents_page.shown);
        app.agents_page.busy.insert("test".into());
        let _ = app.update(Message::Escape);
        assert!(!app.backend_shown());
        assert!(app.agents_page.busy.contains("test"));
        let _ = app.update(msg(Event::Done(
            "test".into(),
            Err("install failed".into()),
        )));
        assert!(!app.agents_page.busy.contains("test"));
        assert_eq!(app.agents_page.logs.get("test").unwrap(), "install failed");
        assert!(
            !app.backend_shown(),
            "late completion must not reopen the page"
        );
    }
    #[test]
    fn live_install_requires_confirmation_before_starting_work() {
        let (mut app, _) = App::new(true, None);
        let registry = serde_json::from_value(serde_json::json!({
            "entry_id":"live-agent", "name":"Live", "version":"1", "source":"live",
            "installable":true, "description":"", "repository":null, "website":null,
            "install_kind":"npx", "install_spec":"some-package@1", "package_name":"some-package",
            "has_icon":false, "builtin_agent_id":null, "newer":null
        }))
        .unwrap();
        app.agents_page.entries.push(Entry {
            registry,
            installed: None,
            definition: None,
            available: false,
            discovery: None,
        });
        let _ = app.update(msg(Event::Install("live-agent".into())));
        assert!(app.agents_page.busy.is_empty());
        assert_eq!(
            app.agents_page.armed.as_deref(),
            Some("install:live-agent:1:live")
        );
        assert!(app.agents_page.logs["live-agent"].contains("some-package@1"));
        let _ = app.update(msg(Event::Edit("live-agent".into())));
        assert_eq!(app.agents_page.details.as_deref(), Some("live-agent"));
        let _ = app.update(msg(Event::Edit("live-agent".into())));
        assert!(app.agents_page.details.is_none());
        assert!(app.agents_page.editor.is_none());
        app.agents_page.entries[0].definition = Some(
            serde_json::from_value(serde_json::json!({
                "id":"live-agent", "name":"Live", "command":["agent", "--acp"]
            }))
            .unwrap(),
        );
        let _ = app.update(msg(Event::Edit("live-agent".into())));
        assert!(app.agents_page.editor.is_some());
        let _ = app.update(msg(Event::Edit("live-agent".into())));
        assert!(app.agents_page.editor.is_none());
        assert!(app.agents_page.details.is_none());
    }
    #[test]
    fn install_follows_the_per_agent_version_choice() {
        let (mut app, _) = App::new(true, None);
        let registry: RegistryEntry = serde_json::from_value(serde_json::json!({
            "entry_id":"codex-acp", "name":"Codex", "version":"1", "source":"snapshot",
            "installable":true, "description":"", "repository":null, "website":null,
            "install_kind":"npx", "install_spec":"codex@1", "package_name":"codex",
            "has_icon":false, "builtin_agent_id":null, "newer":{
                "entry_id":"codex-acp", "name":"Codex", "version":"2", "source":"live",
                "installable":true, "description":"", "repository":null, "website":null,
                "install_kind":"npx", "install_spec":"codex@2", "package_name":"codex",
                "has_icon":false, "builtin_agent_id":null, "newer":null
            }
        }))
        .unwrap();
        app.agents_page.entries.push(Entry {
            registry,
            installed: None,
            definition: None,
            available: false,
            discovery: None,
        });
        // Latest by default: the newer live version, which asks for confirmation first
        assert!(app.settings.prefers_latest("codex-acp"));
        assert_eq!(target(&app.settings, &app.agents_page.entries[0]).version, "2");
        let _ = app.update(msg(Event::Install("codex-acp".into())));
        assert_eq!(
            app.agents_page.armed.as_deref(),
            Some("install:codex-acp:2:live")
        );
        assert!(app.agents_page.logs["codex-acp"].contains("codex@2"));
        // Supported: the snapshot's version, no confirmation needed
        let _ = app.update(msg(Event::Latest("codex-acp".into(), false)));
        assert!(!app.settings.prefers_latest("codex-acp"));
        assert_eq!(target(&app.settings, &app.agents_page.entries[0]).version, "1");
        app.agents_page.armed = None;
        app.agents_page.logs.clear();
        let _ = app.update(msg(Event::Install("codex-acp".into())));
        assert!(app.agents_page.armed.is_none());
        assert!(!app.agents_page.logs.contains_key("codex-acp"));
        let _ = app.update(msg(Event::Latest("codex-acp".into(), true)));
        assert!(app.settings.supported_agents.is_empty());
    }
    #[test]
    fn directory_setup_saves_the_ticked_proposals_as_home_paths() {
        use crate::tests::{connector, daemon, settle_task};
        use serde_json::json;
        let (mut app, _) = App::new(true, None);
        let registry: RegistryEntry = serde_json::from_value(json!({
            "entry_id":"antigravity-acp", "name":"Antigravity", "version":"1", "source":"snapshot",
            "installable":true, "description":"", "repository":null, "website":null,
            "install_kind":"npx", "install_spec":"antigravity@1", "package_name":"antigravity",
            "has_icon":false, "builtin_agent_id":null, "newer":null
        }))
        .unwrap();
        let definition: Definition = serde_json::from_value(json!({
            "id":"antigravity-acp", "name":"Antigravity", "command":["antigravity-acp"]
        }))
        .unwrap();
        assert!(definition.home_paths.is_none());
        let installed = serde_json::from_value(json!({"version":"1","kind":"npx","command":["antigravity-acp"],"env":{},"source":"snapshot"})).unwrap();
        let mut entry = Entry { registry, installed: Some(installed), definition: Some(definition), available: true, discovery: None };
        assert!(entry.needs_setup());
        // nothing discovered yet: Apply does nothing, the card offers Find directories
        app.agents_page.entries.push(entry.clone());
        let _ = app.update(msg(Event::ApplySetup("antigravity-acp".into())));
        assert!(app.agents_page.busy.is_empty());
        entry.discovery = Some(serde_json::from_value(json!({"at":"1", "version":"1", "proposals":[
            {"path":"~/.antigravity", "kind":"directory", "on_host":false},
            {"path":"~/.antigravity.json", "kind":"file", "on_host":false, "initial_content":"{}"},
            {"path":"~/.cargo", "kind":"directory", "on_host":true}
        ], "auth_methods":[{"id":"oauth-personal","name":"Log in with Google"}], "login_method":"oauth-personal",
           "login_files":[".antigravity/oauth_creds.json", ".cargo/credentials.toml"]})).unwrap());
        app.agents_page.entries[0] = entry;
        let _ = app.update(msg(Event::Choose("antigravity-acp".into(), "~/.cargo".into(), false)));
        let _ = app.update(msg(Event::Choose("antigravity-acp".into(), "~/.antigravity".into(), false)));
        let _ = app.update(msg(Event::Choose("antigravity-acp".into(), "~/.antigravity".into(), true)));
        let (client, mut daemon) = daemon();
        app.actions = Some(sill_client::actions::Actions::new(connector(vec![client])));
        let worker = std::thread::spawn(move || {
            let req = daemon.expect("_sill/agents.save");
            assert_eq!(req["params"]["id"], "antigravity-acp");
            assert_eq!(req["params"]["command"], json!(["antigravity-acp"]));
            assert_eq!(
                req["params"]["home_paths"],
                json!([{"path":"~/.antigravity","kind":"directory"}, {"path":"~/.antigravity.json","kind":"file","initial_content":"{}"}])
            );
            assert_eq!(req["params"]["login_files"], json!([".antigravity/oauth_creds.json"]), "only under ticked directories");
            assert_eq!(req["params"]["host_login"], json!(["oauth-personal"]), "a browser login stays on the host");
            daemon.reply(&req, json!({"path":{"host":"local","path":"/home/x/.config/sill/agents/antigravity-acp.toml"}}));
        });
        let task = app.update(msg(Event::ApplySetup("antigravity-acp".into())));
        settle_task(&mut app, task);
        worker.join().unwrap();
        assert!(!app.agents_page.unticked.contains_key("antigravity-acp"), "a saved setup forgets the ticks");
    }
    #[test]
    #[ignore = "writes Agents page PNGs for visual review"]
    fn render_agents_page() {
        let (mut app, _) = App::new(true, None);
        app.agents_page.shown = true;
        app.agents_page.untested = true;
        for (id, name, source) in [
            ("codex-acp", "Codex", "snapshot"),
            ("example-agent", "Example agent with a longer name", "live"),
        ] {
            let registry = serde_json::from_value(serde_json::json!({
                "entry_id":id,"name":name,"version":"1.2.3","source":source,
                "installable":true,"description":"An agent that works with your code, tools and repository.",
                "repository":"https://github.com/example/agent","website":null,
                "install_kind":"npx","install_spec":"example@1.2.3","package_name":"example",
                "has_icon":false,"builtin_agent_id":null,"newer":null
            })).unwrap();
            let definition = serde_json::from_value(serde_json::json!({
                "id":id,"name":name,"command":["agent", "--acp"],"env":{"API_KEY":"example"},
                "domains":["openai.com", "github.com", "google.com"]
            }))
            .unwrap();
            app.agents_page.entries.push(Entry {
                registry,
                definition: Some(definition),
                installed: None,
                available: false,
                discovery: None,
            });
        }
        crate::snapshots::render(&app, "compact-agents");
        crate::snapshots::render_size(&app, "narrow-compact-agents", (480.0, 860.0));
        let _ = app.update(msg(Event::Edit("codex-acp".into())));
        assert!(app.agents_page.editor.is_some());
        crate::snapshots::render(&app, "agents");
        crate::snapshots::render_size(&app, "agents-narrow", (480.0, 860.0));
        let _ = app.update(msg(Event::Edit("codex-acp".into())));
        assert!(app.agents_page.editor.is_none());
        assert!(app.agents_page.details.is_none());
    }
    #[test]
    fn switching_backend_pages_and_sessions_leaves_agents() {
        let (mut app, _) = App::new(true, None);
        let _ = app.update(Message::ShowPage(crate::Page::Agents));
        let _ = app.update(Message::ShowPage(crate::Page::Settings));
        assert!(!app.agents_page.shown);
        assert!(app.settings_page.is_some());
        let _ = app.update(Message::ShowPage(crate::Page::Agents));
        assert!(app.settings_page.is_none());
        let id = app.tabs[0].id.clone();
        let _ = app.update(Message::Choose(id));
        assert!(!app.backend_shown());
    }
}
