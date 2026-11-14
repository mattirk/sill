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
}

pub(super) struct PreparedPrompt {
    pub text: String,
    standing: String,
}

impl Context {
    pub fn prepare(&self, store: &Store, id: &str, home: &Path, text: &str) -> Result<PreparedPrompt> {
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
        // Global pins are mounts, not the session's focus or recall scope (docs/pins.md).
        let pins = store.pins(id)?;
        let memory = if session.ghost && !cfg.ghost_memory() {
            String::new()
        } else {
            sill_core::memory::recall(store, &session, &pins, home, cfg.memory_recall())
        };
        let standing = sill_core::instructions::standing_block(&facts, &memory, &sill_core::instructions::load_global());
        Ok(self.prepare_parts(text, standing, &pins))
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
        self.reconcile_model = None;
    }

    pub fn restoring(&self) -> bool {
        self.reconcile_model.is_some()
    }

    /// Record that a person selected a model. The subsequent Controls event must reapply their
    /// remembered mode/effort against that model, just as reconnect restoration does.
    pub fn chosen(&mut self, controls: Option<&Controls>, command: &acp::Command) {
        if let acp::Command::SetConfig(id, ConfigValue::Id(value)) = command
            && controls.is_some_and(|c| c.options.iter().any(|o| o.id == *id && o.category == "model"))
        {
            self.reconcile_model = Some(value.clone());
        }
    }

    /// `initial` is true for Ready, false for later Controls events. Only a matching model
    /// acknowledgement triggers the second restoration pass, avoiding echoes for ordinary updates.
    pub fn restore(&mut self, controls: &Controls, remembered: &AgentSettings, initial: bool) -> Vec<acp::Command> {
        if !initial {
            let Some(model) = self.reconcile_model.as_ref() else { return Vec::new() };
            if !controls.options.iter().any(|o| o.category == "model" && matches!(&o.kind, ConfigKind::Select(current, _) if current == model)) {
                return Vec::new();
            }
            self.reconcile_model = None;
        }
        // Model first, and wait for its response before selecting values from its option set.
        let models: Vec<_> = controls.options.iter().filter(|o| o.category == "model").filter_map(|o| wanted(o, remembered)).collect();
        if !models.is_empty() {
            if let Some(acp::Command::SetConfig(_, ConfigValue::Id(value))) = models.last() {
                self.reconcile_model = Some(value.clone());
            }
            return models;
        }
        let mut commands = Vec::new();
        if let (Some(mode), Some((current, modes))) = (&remembered.mode, &controls.modes)
            && mode != current
            && modes.iter().any(|(id, _)| id == mode)
        {
            commands.push(acp::Command::SetMode(mode.clone()));
        }
        commands.extend(controls.options.iter().filter(|o| o.category != "model").filter_map(|o| wanted(o, remembered)));
        commands
    }
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

fn pin_prefix(pins: &[PathBuf]) -> String {
    let (dirs, files): (Vec<_>, Vec<_>) = pins.iter().partition(|p| p.is_dir());
    let mut out = String::new();
    for (paths, label) in [(dirs, "Pinned folders — focus your work there unless told otherwise"), (files, "Pinned files — the person keeps these at hand")] {
        if !paths.is_empty() {
            out.push_str(&format!("[{label}: {}]\n", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
        }
    }
    if !pins.is_empty() {
        out.push_str("[Pin changes take effect when the jail restarts. A folder pinned during this run may still use the session overlay; verify its mount before assuming writes reach the host.]\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(after.len(), 2);
        assert!(matches!(&after[0], acp::Command::SetMode(mode) if mode == "auto"));
        assert!(matches!(&after[1], acp::Command::SetConfig(id, ConfigValue::Id(v)) if id == "effort" && v == "high"));
        assert!(context.restore(&controls("large", "low"), &settings, false).is_empty());
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
