//! Session behavior shared by every frontend: prompt context and remembered controls.
//!
//! This state belongs to a live agent conversation, never to a window. A prepared prompt is
//! committed only after it reaches the command queue, so a refused send cannot consume instructions.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sill_core::acp::{self, ConfigKind, ConfigOption, ConfigValue, Controls};
use sill_core::store::{AgentSettings, Store};

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Context {
    sent_standing: Option<String>,
    /// Changing model rebuilds its available options. Restore the other choices only after the
    /// agent confirms the model, using the newly offered values.
    reconcile_model: Option<String>,
    /// The option `reconcile_model` was requested through, so its refusal can be told apart.
    #[serde(default)]
    reconcile_option: Option<String>,
    #[serde(default)]
    pending_controls: Option<AgentSettings>,
    #[serde(default)]
    issued_controls: std::collections::BTreeSet<String>,
    #[serde(default)]
    controls_deadline: Option<u64>,
    #[serde(default)]
    pub controls_error: Option<String>,
    /// The pending settings are a person's own picks, shown at once and left usable; false while
    /// saved settings are restored into a starting agent, which holds the controls.
    #[serde(default)]
    user_controls: bool,
}

pub(super) struct PreparedPrompt {
    pub text: String,
    standing: String,
}

impl Context {
    pub fn prepare(&self, store: &Store, id: &str, home: &Path, text: &str, live: &[String]) -> Result<PreparedPrompt> {
        let Some(session) = store.sessions()?.into_iter().find(|s| s.id == id) else { bail!("no session {id}") };
        let cfg = sill_core::config::load();
        // what the jail was built with (session choices included), else what the next build uses
        let launched = sill_core::session::policy_snapshot(id).ok().flatten().map(|launch| launch.policy);
        let facts = launched
            .map(Ok)
            .unwrap_or_else(|| sill_core::session::policy_of(&session, home))
            .map(|policy| {
                let mut facts = sill_core::instructions::facts_block(&policy, home);
                if session.ghost {
                    facts.push_str(sill_core::instructions::GHOST_NOTICE);
                }
                facts
            })
            .unwrap_or_default();
        let facts = match sill_core::workspace::records::session_notice(store.conn(), id) {
            Ok(Some(notice)) => facts + &notice,
            _ => facts,
        };
        // Only session pins define focus and recall scope; shortcuts are navigation.
        let pins = store.pins(id)?;
        let memory = if session.ghost && !cfg.ghost_memory() {
            String::new()
        } else {
            sill_core::memory::recall(store, &session, &pins, home, cfg.memory_recall())
        };
        let standing = sill_core::instructions::standing_block(&facts, &memory, &sill_core::instructions::load_global());
        let notice = parallel_notice(store, id, &pins, live)?;
        Ok(self.prepare_parts(&format!("{notice}{text}"), standing, &pins))
    }

    fn prepare_parts(&self, text: &str, standing: String, pins: &[PathBuf]) -> PreparedPrompt {
        let mut outgoing = if self.sent_standing.as_ref() == Some(&standing) { String::new() } else { standing.clone() };
        outgoing.push_str(&pin_prefix(pins));
        if !outgoing.is_empty() {
            outgoing.push('\n');
        }
        outgoing.push_str(text);
        PreparedPrompt { text: outgoing, standing }
    }

    pub fn commit(&mut self, prepared: PreparedPrompt) {
        self.sent_standing = Some(prepared.standing);
    }

    pub fn ready(&mut self, resumed: bool) {
        if !resumed {
            self.sent_standing = None;
        }
        self.settle();
        self.issued_controls.clear();
        self.controls_error = None;
    }

    pub fn restoring(&self) -> bool {
        self.pending_controls.is_some()
    }

    /// Saved settings are being restored: the controls show the last confirmed values, disabled.
    pub fn locking(&self) -> bool {
        self.restoring() && !self.user_controls
    }

    /// A person's picks the agent has not confirmed yet, to show over its reported values.
    pub fn optimistic(&self) -> Option<&AgentSettings> {
        self.pending_controls.as_ref().filter(|_| self.user_controls)
    }

    fn begin_controls(&mut self, settings: AgentSettings, user: bool) {
        self.pending_controls = Some(settings);
        self.issued_controls.clear();
        self.controls_error = None;
        self.controls_deadline = Some(control_now() + 20_000);
        self.user_controls = user;
    }

    pub fn expire_controls(&mut self, now: u64) -> bool {
        if !self.restoring() || !self.controls_deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        let pending = self.pending_controls.take().unwrap();
        let mut names: Vec<_> = pending.config.keys().cloned().collect();
        if pending.mode.is_some() { names.push("mode".into()); }
        self.controls_error = Some(format!("Agent did not confirm settings: {}. Showing reported values.", names.join(", ")));
        self.settle();
        true
    }

    /// The agent refused one setting (`mode` or `config:<option id>`): only that one is let go.
    /// The others stay pending; after a refused model the caller runs `restore` again, so the
    /// settings that waited for it go out against the model the agent kept.
    pub fn fail_control(&mut self, setting: &str, reason: &str) {
        self.controls_error = Some(format!("{reason}. Showing reported values."));
        let Some(pending) = self.pending_controls.as_mut() else { return };
        match setting.strip_prefix("config:") {
            Some(id) => {
                pending.config.remove(id);
                if self.reconcile_option.as_deref() == Some(id) { self.reconcile_model = None; self.reconcile_option = None; }
            }
            None => pending.mode = None,
        }
        if pending.mode.is_none() && pending.config.is_empty() { self.settle(); }
    }

    fn settle(&mut self) {
        self.pending_controls = None;
        self.reconcile_model = None;
        self.reconcile_option = None;
        self.controls_deadline = None;
        self.user_controls = false;
    }

    /// Keep user selections pending until the agent reports them. Model changes also reconcile
    /// the remaining saved choices against the new model's option schema. A pick made while an
    /// earlier one is still unconfirmed joins it; the caller refuses picks while `locking`.
    pub fn chosen(&mut self, controls: Option<&Controls>, command: &acp::Command) {
        let mut settings = AgentSettings::default();
        match command {
            acp::Command::SetMode(mode) => settings.mode = Some(mode.clone()),
            acp::Command::SetConfig(id, value) => {
                let value = match value { ConfigValue::Id(v) => v.clone(), ConfigValue::Bool(v) => v.to_string() };
                settings.config.insert(id.clone(), value.clone());
                if controls.is_some_and(|c| c.options.iter().any(|o| o.id == *id && o.category == "model")) {
                    self.reconcile_model = Some(value);
                    self.reconcile_option = Some(id.clone());
                }
            }
            _ => return,
        }
        if let Some(pending) = self.pending_controls.as_mut().filter(|_| self.user_controls) {
            if settings.mode.is_some() { pending.mode = settings.mode; }
            pending.config.extend(settings.config);
            self.controls_error = None;
            self.controls_deadline = Some(control_now() + 20_000);
        } else {
            self.begin_controls(settings, true);
        }
        self.issued_controls.insert(match command { acp::Command::SetMode(_) => "mode".into(), acp::Command::SetConfig(id, _) => format!("config:{id}"), _ => unreachable!() });
    }

    pub fn restore(&mut self, controls: &Controls, remembered: &AgentSettings, initial: bool) -> Vec<acp::Command> {
        if initial { self.begin_controls(remembered.clone(), false); }
        if !self.restoring() { return Vec::new(); }
        if let Some(model) = &self.reconcile_model {
            if !controls.options.iter().any(|o| o.category == "model" && option_current(o) == *model) {
                return Vec::new();
            }
            self.reconcile_model = None;
            self.reconcile_option = None;
            self.issued_controls.clear();
            // The selected model is now confirmed. Restore dependent choices only now.
            self.pending_controls = Some(remembered.clone());
        }
        let pending = self.pending_controls.as_mut().unwrap();
        let mut commands = Vec::new();
        let mut unsupported = Vec::new();
        // Models first. Do not validate effort against the old model's choices.
        let model_ids: Vec<_> = controls.options.iter().filter(|o| o.category == "model").map(|o| o.id.clone()).collect();
        for id in &model_ids {
            let Some(want) = pending.config.get(id).cloned() else { continue };
            let option = controls.options.iter().find(|o| o.id == *id).unwrap();
            if option_current(option) == want { pending.config.remove(id); continue; }
            if let Some(command) = wanted(option, pending) {
                if self.issued_controls.insert(format!("config:{id}")) { commands.push(command); }
                self.reconcile_model = Some(want);
                self.reconcile_option = Some(id.clone());
                return commands;
            }
            unsupported.push(id.clone()); pending.config.remove(id);
        }
        // The model is settled, so what the agent reports now is what it offers. A saved setting
        // with no such control (an effort level on a model that has none) is not waited for.
        pending.config.retain(|id, _| controls.options.iter().any(|o| o.id == *id));
        if controls.modes.is_none() { pending.mode = None; }
        if let (Some(want), Some((current, values))) = (&pending.mode, &controls.modes) {
            if want == current { pending.mode = None; }
            else if !values.iter().any(|(id, _)| id == want) { unsupported.push("mode".into()); pending.mode = None; }
            else if self.issued_controls.insert("mode".into()) { commands.push(acp::Command::SetMode(want.clone())); }
        }
        for option in &controls.options {
            let Some(want) = pending.config.get(&option.id) else { continue };
            if option_current(option) == *want { pending.config.remove(&option.id); }
            else if let Some(command) = wanted(option, pending) {
                if self.issued_controls.insert(format!("config:{}", option.id)) { commands.push(command); }
            } else { unsupported.push(option.id.clone()); pending.config.remove(&option.id); }
        }
        if !unsupported.is_empty() {
            self.controls_error = Some(format!("Saved settings unavailable: {}. Showing reported values.", unsupported.join(", ")));
        }
        if pending.mode.is_none() && pending.config.is_empty() { self.settle(); }
        commands
    }

}

pub(super) fn control_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn option_current(option: &ConfigOption) -> String {
    match &option.kind { ConfigKind::Select(value, _) => value.clone(), ConfigKind::Boolean(value) => value.to_string() }
}

fn wanted(option: &ConfigOption, remembered: &AgentSettings) -> Option<acp::Command> {
    let want = remembered.config.get(&option.id)?;
    let value = match &option.kind {
        ConfigKind::Select(current, values) if current != want && values.iter().any(|(id, _)| id == want) => ConfigValue::Id(want.clone()),
        ConfigKind::Boolean(current) if (want == "true" || want == "false") && (want == "true") != *current => ConfigValue::Bool(want == "true"),
        _ => return None,
    };
    Some(acp::Command::SetConfig(option.id.clone(), value))
}

/// Remember a person's choice in both places atomically: this session and the agent's defaults.
/// Restoration commands bypass this function so reconnecting an old session cannot rewrite the
/// defaults chosen more recently in another session.
pub(super) fn remember_choice(store: &Store, id: &str, command: &acp::Command) -> Result<()> {
    if !matches!(command, acp::Command::SetMode(_) | acp::Command::SetConfig(..)) {
        return Ok(());
    }
    let Some(session) = store.sessions()?.into_iter().find(|s| s.id == id) else { bail!("no session {id}") };
    let mut own = session.agent_settings;
    let mut defaults = store.agent_settings(&session.agent);
    let update = |settings: &mut AgentSettings| match command {
        acp::Command::SetMode(mode) => settings.mode = Some(mode.clone()),
        acp::Command::SetConfig(id, value) => {
            settings.config.insert(id.clone(), match value { ConfigValue::Id(v) => v.clone(), ConfigValue::Bool(v) => v.to_string() });
        }
        _ => {}
    };
    update(&mut own);
    update(&mut defaults);
    // User commands persist their outbox, transcript and settings in one surrounding transaction.
    // Standalone callers still get an atomic session/defaults update.
    let transaction = if store.conn().is_autocommit() { Some(store.conn().unchecked_transaction()?) } else { None };
    store.set_session_settings(id, &own)?;
    store.set_agent_settings(&session.agent, &defaults)?;
    if let Some(transaction) = transaction { transaction.commit()?; }
    Ok(())
}

/// Only shared explicit pins disclose a peer. No prompts, summaries, tools or control handles.
fn parallel_notice(store: &Store, id: &str, pins: &[PathBuf], live: &[String]) -> Result<String> {
    if pins.is_empty() { return Ok(String::new()); }
    let mut rows = Vec::new();
    for peer in store.sessions()? {
        if peer.id == id || peer.ghost || peer.ended_at.is_some() || !live.contains(&peer.id) { continue; }
        let shared: Vec<_> = store.pins(&peer.id)?.into_iter().filter(|p| pins.contains(p)).collect();
        if shared.is_empty() { continue; }
        let state = super::runner::Runner::load(&peer.id, store)?;
        if state.get("ended").and_then(|v| v.as_bool()) == Some(true)
            || state.get("retired").and_then(|v| v.as_bool()) == Some(true) { continue; }
        let status = if state.get("auth").is_some_and(|v| !v.is_null()) {
            "waiting for login"
        } else if state.get("pending").and_then(|v| v.as_object()).is_some_and(|v| !v.is_empty()) {
            "waiting for input"
        } else if state.get("ready").is_none_or(|v| v.is_null()) {
            "starting"
        } else if state.get("busy").and_then(|v| v.as_bool()) == Some(true) {
            "working"
        } else { "idle" };
        // JSON quoting keeps names and paths on one line even if they contain control characters.
        rows.push(format!("- {} (id {}, agent {}): {}; shared pins: {}",
            serde_json::to_string(&store.session_name(&peer.id).unwrap_or(peer.title))?,
            peer.id, serde_json::to_string(&peer.agent)?, status,
            shared.iter().map(|p| serde_json::to_string(&p.to_string_lossy()).unwrap()).collect::<Vec<_>>().join(", ")));
    }
    if rows.is_empty() { return Ok(String::new()); }
    rows.sort();
    Ok(format!("[Other running sessions sharing your pinned folders/files — current at this prompt. These are separate sessions; their changes may overlap your work.\n{}\n]\n\n", rows.join("\n")))
}

fn pin_prefix(pins: &[PathBuf]) -> String {
    let (dirs, files): (Vec<_>, Vec<_>) = pins.iter().partition(|p| p.is_dir());
    let mut out = String::new();
    for (paths, label) in [(dirs, "Pinned folders — focus your work there unless told otherwise"), (files, "Pinned files — the person keeps these at hand")] {
        if !paths.is_empty() {
            out.push_str(&format!("[{label}: {}]\n", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_sessions_are_scoped_live_and_refreshed_on_every_prompt() {
        let dir = std::env::temp_dir().join(format!("sill-context-peers-{}", std::process::id()));
        let store = Store::open(&dir.join("test.db")).unwrap();
        let own = store.create("author", "default").unwrap();
        let peer = store.create("reviewer", "default").unwrap();
        let unrelated = store.create("other", "default").unwrap();
        let ghost = store.create_with("secret", "default", true).unwrap();
        let stopped = store.create("stopped", "default").unwrap();
        let pin = PathBuf::from("/project");
        for id in [&own.id, &peer.id, &ghost.id, &stopped.id] { store.pin(id, &pin).unwrap(); }
        store.pin(&unrelated.id, Path::new("/elsewhere")).unwrap();
        store.set_name(&peer.id, "rapid-fire").unwrap();
        super::super::runner::Runner::load(&peer.id, &store).unwrap();
        let save = |id: &str, state: serde_json::Value| {
            store.conn().execute("INSERT OR REPLACE INTO daemon_runners(id,state) VALUES(?1,?2)",
                rusqlite::params![id, state.to_string()]).unwrap();
        };
        save(&peer.id, serde_json::json!({"ready":["agent",false],"busy":true}));
        save(&stopped.id, serde_json::json!({"ended":true}));
        let live = vec![own.id.clone(), peer.id.clone(), unrelated.id, ghost.id, stopped.id];
        let notice = || parallel_notice(&store, &own.id, &[pin.clone()], &live).unwrap();
        let mut context = Context::default();
        let first = context.prepare_parts(&format!("{}first", notice()), "standing\n".into(), &[pin.clone()]);
        assert!(first.text.contains("rapid-fire") && first.text.contains("working"));
        assert!(!first.text.contains("secret") && !first.text.contains("stopped") && !first.text.contains("elsewhere"));
        context.commit(first);
        // The live notice is repeated even after the standing block has been consumed.
        let second = context.prepare_parts(&format!("{}second", notice()), "standing\n".into(), &[pin.clone()]);
        assert!(second.text.contains("rapid-fire") && !second.text.contains("standing"));
        save(&peer.id, serde_json::json!({"ready":["agent",false],"pending":{"1":{}}}));
        assert!(notice().contains("waiting for input"));
        save(&peer.id, serde_json::json!({"retired":true}));
        assert!(notice().is_empty());
        save(&peer.id, serde_json::json!({"busy":true}));
        assert!(parallel_notice(&store, &own.id, &[pin], &[]).unwrap().is_empty(), "a durable row alone is not live");
        assert!(parallel_notice(&store, &own.id, &[], &live).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn controls(model: &str, effort: &str) -> Controls {
        let option = |id: &str, category: &str, current: &str, values: &[&str]| ConfigOption {
            id: id.into(), name: id.into(), category: category.into(),
            kind: ConfigKind::Select(current.into(), values.iter().map(|s| (s.to_string(), s.to_string())).collect()),
        };
        Controls {
            modes: Some(("ask".into(), vec![("ask".into(), "Ask".into()), ("auto".into(), "Auto".into())])),
            options: vec![option("model", "model", model, &["small", "large"]), option("effort", "thought_level", effort, &["low", "high"])],
        }
    }

    #[test]
    fn instructions_commit_only_after_send_and_follow_changes_and_agent_lifetime() {
        let mut context = Context::default();
        let standing = "facts\nrecall\ninstructions\n".to_string();
        let failed = context.prepare_parts("plain user text", standing.clone(), &[]);
        assert!(failed.text.contains("instructions"));
        let accepted = context.prepare_parts("plain user text", standing.clone(), &[]);
        assert!(accepted.text.contains("instructions"), "a failed send consumes nothing");
        context.commit(accepted);
        assert_eq!(context.prepare_parts("second", standing.clone(), &[]).text, "second");
        context.ready(true);
        assert_eq!(context.prepare_parts("resumed", standing.clone(), &[]).text, "resumed");
        assert!(context.prepare_parts("changed", format!("{standing}new recall\n"), &[]).text.contains("new recall"));
        context.ready(false);
        assert!(context.prepare_parts("fresh", standing, &[]).text.contains("instructions"));
    }

    #[test]
    fn pins_follow_each_prompt_even_when_standing_is_unchanged() {
        let mut context = Context::default();
        let pin = PathBuf::from("/tmp/sill-a-pinned-file");
        let prompt = context.prepare_parts("first", "standing\n".into(), &[pin.clone()]);
        context.commit(prompt);
        let second = context.prepare_parts("second", "standing\n".into(), &[pin]);
        assert!(!second.text.contains("standing"));
        assert!(second.text.contains("Pinned files"));
        assert!(second.text.ends_with("\n\nsecond"));
    }

    #[test]
    fn model_restoration_waits_then_restores_mode_and_new_options_once() {
        let mut context = Context::default();
        let settings = AgentSettings { mode: Some("auto".into()), config: [("model".into(), "large".into()), ("effort".into(), "high".into())].into() };
        let first = context.restore(&controls("small", "low"), &settings, true);
        assert_eq!(first.len(), 1);
        assert!(matches!(&first[0], acp::Command::SetConfig(id, ConfigValue::Id(v)) if id == "model" && v == "large"));
        assert!(context.restore(&controls("small", "low"), &settings, false).is_empty());
        let after = context.restore(&controls("large", "low"), &settings, false);
        assert!(context.restoring());
        assert_eq!(after.len(), 2);
        assert!(matches!(&after[0], acp::Command::SetMode(mode) if mode == "auto"));
        assert!(matches!(&after[1], acp::Command::SetConfig(id, ConfigValue::Id(v)) if id == "effort" && v == "high"));
        assert!(context.restore(&controls("large", "low"), &settings, false).is_empty());
    }

    #[test]
    fn restoration_tracks_all_acknowledgements_without_resending() {
        let settings = AgentSettings { mode: Some("auto".into()), config: [("effort".into(), "high".into())].into() };
        let mut context = Context::default();
        let mut offered = controls("large", "low");
        assert_eq!(context.restore(&offered, &settings, true).len(), 2);
        assert!(context.locking(), "saved settings hold the controls until confirmed");
        assert!(context.restore(&offered, &settings, false).is_empty(), "never resend on unrelated updates");
        offered.modes.as_mut().unwrap().0 = "auto".into();
        assert!(context.restore(&offered, &settings, false).is_empty());
        assert!(context.restoring(), "one acknowledgement does not finish restoration");
        offered.options[1].kind = ConfigKind::Select("high".into(), vec![]);
        assert!(context.restore(&offered, &settings, false).is_empty());
        assert!(!context.restoring());
    }

    #[test]
    fn a_saved_setting_the_agent_does_not_offer_is_not_waited_for() {
        let settings = AgentSettings { mode: Some("auto".into()), config: [("model".into(), "large".into()), ("effort".into(), "high".into())].into() };
        let mut context = Context::default();
        // the model comes first: nothing is dropped against the old model's controls
        let mut small = controls("small", "low"); small.options.remove(1);
        assert_eq!(context.restore(&small, &settings, true).len(), 1);
        assert!(context.restoring());
        // the confirmed model has no effort control and the agent no modes: both are let go
        let mut large = controls("large", "low"); large.options.remove(1); large.modes = None;
        assert!(context.restore(&large, &settings, false).is_empty());
        assert!(!context.restoring());
        assert!(context.controls_error.is_none(), "a control that does not exist is no failure");
    }

    #[test]
    fn unsupported_choices_and_missing_acknowledgements_finish_with_an_error() {
        let mut context = Context::default();
        let settings = AgentSettings { mode: None, config: [("effort".into(), "impossible".into())].into() };
        assert!(context.restore(&controls("large", "low"), &settings, true).is_empty());
        assert!(!context.restoring());
        assert!(context.controls_error.as_ref().unwrap().contains("effort"));
        let settings = AgentSettings { mode: Some("auto".into()), config: [("effort".into(), "high".into())].into() };
        context.restore(&controls("large", "low"), &settings, true);
        assert!(!context.expire_controls(0));
        assert!(context.expire_controls(u64::MAX));
        assert!(!context.restoring());
        assert!(context.controls_error.as_ref().unwrap().contains("effort"));
        assert!(context.controls_error.as_ref().unwrap().contains("mode"));
        assert!(!context.expire_controls(u64::MAX));
    }

    #[test]
    fn a_persons_picks_show_at_once_join_each_other_and_do_not_hold_the_controls() {
        let mut context = Context::default();
        let offered = controls("large", "low");
        context.chosen(Some(&offered), &acp::Command::SetConfig("effort".into(), ConfigValue::Id("high".into())));
        assert!(context.restoring() && !context.locking());
        context.chosen(Some(&offered), &acp::Command::SetMode("auto".into()));
        let picked = context.optimistic().unwrap();
        assert_eq!(picked.mode.as_deref(), Some("auto"));
        assert_eq!(picked.config.get("effort").map(String::as_str), Some("high"));
        let settings = picked.clone();
        // the first acknowledgement keeps the second pick pending, and nothing is sent twice
        assert!(context.restore(&controls("large", "high"), &settings, false).is_empty());
        assert!(context.optimistic().is_some_and(|p| p.mode.is_some() && p.config.is_empty()));
        let mut confirmed = controls("large", "high"); confirmed.modes.as_mut().unwrap().0 = "auto".into();
        assert!(context.restore(&confirmed, &settings, false).is_empty());
        assert!(!context.restoring() && context.optimistic().is_none());
        // a rejection lets go of the pick
        context.chosen(Some(&offered), &acp::Command::SetMode("auto".into()));
        context.fail_control("mode", "set mode failed");
        assert!(context.optimistic().is_none() && !context.restoring());
    }

    #[test]
    fn a_refusal_lets_go_of_its_own_setting_only() {
        // a refused model: mode and effort still go out, against the model the agent kept
        let settings = AgentSettings { mode: Some("auto".into()), config: [("model".into(), "large".into()), ("effort".into(), "high".into())].into() };
        let mut context = Context::default();
        assert_eq!(context.restore(&controls("small", "low"), &settings, true).len(), 1);
        context.fail_control("config:model", "set option failed");
        assert!(context.restoring());
        let after = context.restore(&controls("small", "low"), &settings, false);
        assert!(matches!(&after[..], [acp::Command::SetMode(mode), acp::Command::SetConfig(id, _)] if mode == "auto" && id == "effort"));
        let mut confirmed = controls("small", "high"); confirmed.modes.as_mut().unwrap().0 = "auto".into();
        context.restore(&confirmed, &settings, false);
        assert!(!context.restoring());
        assert!(context.controls_error.as_ref().unwrap().contains("set option failed"), "the refusal stays reported");
        // one of two picks refused: the other is still shown and awaited
        let offered = controls("large", "low");
        context.chosen(Some(&offered), &acp::Command::SetConfig("effort".into(), ConfigValue::Id("high".into())));
        context.chosen(Some(&offered), &acp::Command::SetMode("auto".into()));
        context.fail_control("mode", "set mode failed");
        let picked = context.optimistic().unwrap();
        assert!(picked.mode.is_none() && picked.config.contains_key("effort"));
        // a refused effort during a model change does not end the wait for the model
        let mut context = Context::default();
        context.chosen(Some(&offered), &acp::Command::SetConfig("model".into(), ConfigValue::Id("small".into())));
        context.chosen(Some(&offered), &acp::Command::SetConfig("effort".into(), ConfigValue::Id("high".into())));
        context.fail_control("config:effort", "set option failed");
        assert!(context.restore(&offered, &settings, false).is_empty(), "the model is not confirmed yet");
        assert!(context.restoring());
    }

    #[test]
    fn user_choice_waits_for_confirmation_and_survives_serialization() {
        let mut context = Context::default();
        let command = acp::Command::SetConfig("effort".into(), ConfigValue::Id("high".into()));
        context.chosen(Some(&controls("large", "low")), &command);
        let mut context: Context = serde_json::from_value(serde_json::to_value(&context).unwrap()).unwrap();
        assert!(context.restoring());
        let settings = AgentSettings { mode: None, config: [("effort".into(), "high".into())].into() };
        assert!(context.restore(&controls("large", "low"), &settings, false).is_empty());
        assert!(context.restoring());
        context.restore(&controls("large", "high"), &settings, false);
        assert!(!context.restoring());
    }

    #[test]
    fn choices_persist_for_every_client_without_overwriting_other_defaults() {
        let dir = std::env::temp_dir().join(format!("sill-context-settings-{}", std::process::id()));
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("test-agent", "default").unwrap();
        let defaults = AgentSettings { mode: Some("ask".into()), config: [("other".into(), "kept".into())].into() };
        store.set_agent_settings("test-agent", &defaults).unwrap();
        remember_choice(&store, &session.id, &acp::Command::SetMode("auto".into())).unwrap();
        remember_choice(&store, &session.id, &acp::Command::SetConfig("enabled".into(), ConfigValue::Bool(true))).unwrap();
        let own = store.sessions().unwrap().into_iter().find(|s| s.id == session.id).unwrap().agent_settings;
        assert_eq!(own.mode.as_deref(), Some("auto"));
        assert_eq!(own.config.get("enabled").map(String::as_str), Some("true"));
        let defaults = store.agent_settings("test-agent");
        assert_eq!(defaults.mode.as_deref(), Some("auto"));
        assert_eq!(defaults.config.get("other").map(String::as_str), Some("kept"));
        assert_eq!(defaults.config.get("enabled").map(String::as_str), Some("true"));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembered_choices_follow_the_command_transactions_rollback() {
        let dir = std::env::temp_dir().join(format!("sill-context-rollback-{}", std::process::id()));
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("test-agent", "default").unwrap();
        let transaction = store.conn().unchecked_transaction().unwrap();
        remember_choice(&store, &session.id, &acp::Command::SetMode("auto".into())).unwrap();
        assert_eq!(store.agent_settings("test-agent").mode.as_deref(), Some("auto"));
        transaction.rollback().unwrap();
        assert!(store.agent_settings("test-agent").mode.is_none());
        assert!(store.sessions().unwrap().into_iter().find(|s| s.id == session.id).unwrap().agent_settings.mode.is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
