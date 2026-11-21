//! Policy capability defaults and ceilings. Host authority is always an explicit activation.
use crate::policy::{NetMode, Network};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    #[default]
    Off,
    On,
    Forbidden,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub test_desktop: Rule,
    pub private_bus: Rule,
    pub gpu: Rule,
    pub public_web: Rule,
    pub host_control: Rule,
}

/// Capabilities that make a session Desktop development or Host control rather than Default.
/// Frontends without permission-mode support cannot operate such a session.
pub const MODE_CAPABILITIES: [&str; 4] = ["test_desktop", "private_bus", "gpu", "host_control"];
/// What Apply development permissions turns on together: the test desktop and the private bus.
/// GPU rendering stays an explicit additional choice; Host control is never part of a group.
pub const DEVELOPMENT_CAPABILITIES: [&str; 2] = ["test_desktop", "private_bus"];

/// Whether this machine can enforce `capability`: the public web proxy always; the test desktop
/// when a usable Weston with Xwayland is installed (`desktop::probe`); the private bus when
/// `dbus-daemon` is (`bus::probe`); GPU when a supported render node is accessible.
pub fn available(capability: &str) -> bool {
    match capability {
        "public_web" => true,
        "gpu" => crate::gpu::probe().available(),
        "test_desktop" => crate::desktop::probe().available(),
        "private_bus" => crate::bus::probe().available(),
        _ => false,
    }
}

/// Why `capability` has no enforcement backend here.
fn unavailable_reason(capability: &str) -> String {
    match capability {
        "gpu" => crate::gpu::probe().missing,
        "test_desktop" => crate::desktop::probe().missing.clone(),
        "private_bus" => crate::bus::probe().missing.clone(),
        _ => "enforcement backend not implemented".into(),
    }
}

/// A remembered session choice and what one resolution made of it. Launch snapshots keep
/// these as evidence of the choices a build consulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChoice {
    pub capability: String,
    pub enabled: bool,
    /// The policy default before this choice, after ghost restrictions.
    pub policy_default: bool,
    /// Why the choice did not take effect; never forced past a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

pub struct State {
    pub capability: &'static str,
    pub allowed: bool,
    pub default_enabled: bool,
    pub requested: Option<bool>,
    pub blocked: bool,
    pub source: &'static str,
    pub effective: bool,
    pub available: bool,
    pub reason: String,
}

impl Permissions {
    pub fn rules(&self) -> [(&'static str, Rule); 5] {
        [
            ("test_desktop", self.test_desktop),
            ("private_bus", self.private_bus),
            ("gpu", self.gpu),
            ("public_web", self.public_web),
            ("host_control", self.host_control),
        ]
    }

    pub fn rule(&self, capability: &str) -> Option<Rule> {
        self.rules().into_iter().find(|(name, _)| *name == capability).map(|(_, rule)| rule)
    }

    fn rule_mut(&mut self, capability: &str) -> Option<&mut Rule> {
        match capability {
            "test_desktop" => Some(&mut self.test_desktop),
            "private_bus" => Some(&mut self.private_bus),
            "gpu" => Some(&mut self.gpu),
            "public_web" => Some(&mut self.public_web),
            "host_control" => Some(&mut self.host_control),
            _ => None,
        }
    }

    pub fn validate(&self, network: &Network) -> Result<()> {
        ensure!(self.host_control != Rule::On, "Host control cannot be enabled by a policy default; explicit activation is required");
        ensure!(self.public_web != Rule::Forbidden || network.mode != NetMode::All, "public_web is forbidden but network.mode is all");
        Ok(())
    }

    /// `network.mode = "all"` is the public-web default under another name; report it as one.
    pub fn apply_defaults(&mut self, network: &mut Network) {
        if self.public_web == Rule::On && network.mode != NetMode::None {
            network.mode = NetMode::All;
        }
        if self.public_web == Rule::Off && network.mode == NetMode::All {
            self.public_web = Rule::On;
        }
    }

    pub fn validate_launch(&self, network: &Network) -> Result<()> {
        self.validate(network)?;
        for (name, rule) in self.rules() {
            ensure!(available(name) || rule != Rule::On, "{name} was requested but {}", unavailable_reason(name));
        }
        Ok(())
    }

    /// Whether a session's next or recorded build requests Desktop development or Host control.
    pub fn uses_modes(&self) -> bool {
        MODE_CAPABILITIES.iter().any(|name| self.rule(name) == Some(Rule::On))
    }

    /// Why a human session choice to enable `capability` cannot take effect within these
    /// resolved limits. Host control is an activation, never a remembered choice.
    pub fn enable_refusal(&self, capability: &str, network: &Network) -> Option<&'static str> {
        let rule = self.rule(capability)?;
        if capability == "host_control" {
            return Some("Host control needs explicit activation and is never restored from a saved choice");
        }
        if rule == Rule::Forbidden {
            return Some("forbidden by policy or session restrictions");
        }
        if !available(capability) {
            return Some(match capability {
                "gpu" => "no accessible Intel/AMD GPU render node",
                "test_desktop" => "no test desktop on this machine (Weston and Xwayland)",
                "private_bus" => "no dbus-daemon on this machine (the dbus package)",
                _ => "enforcement backend not implemented",
            });
        }
        // the network limits concern the public web only
        if capability != "public_web" {
            return None;
        }
        if network.mode == NetMode::None {
            return Some("network disabled by resolved policy");
        }
        if network.mode != NetMode::All && !network.allow_additional_hosts {
            return Some("additional hosts are forbidden by policy");
        }
        None
    }

    /// Apply remembered session choices after ghost restrictions. Turning a
    /// capability off always applies; turning one on applies only within the limits above.
    pub fn apply_session(&mut self, network: &mut Network, choices: &BTreeMap<String, bool>) -> Vec<SessionChoice> {
        let mut applied = Vec::new();
        for (capability, &enabled) in choices {
            let Some(rule) = self.rule(capability) else {
                applied.push(SessionChoice { capability: capability.clone(), enabled, policy_default: false, blocked: Some("unknown capability".into()) });
                continue;
            };
            let blocked = if enabled { self.enable_refusal(capability, network) } else { None };
            if blocked.is_none() {
                let target = self.rule_mut(capability).expect("known capability");
                if enabled {
                    *target = Rule::On;
                } else if *target == Rule::On {
                    *target = Rule::Off;
                }
                if capability == "public_web" {
                    if enabled {
                        network.mode = NetMode::All;
                    } else if network.mode == NetMode::All {
                        network.mode = NetMode::Allow;
                    }
                }
            }
            applied.push(SessionChoice { capability: capability.clone(), enabled, policy_default: rule == Rule::On, blocked: blocked.map(Into::into) });
        }
        applied
    }

    pub fn states(&self, network: &Network, choices: &[SessionChoice]) -> Vec<State> {
        self.rules()
            .into_iter()
            .map(|(capability, rule)| {
                let choice = choices.iter().find(|choice| choice.capability == capability);
                let available = available(capability);
                let effective = available
                    && match capability {
                        "public_web" => network.mode == NetMode::All,
                        _ => rule == Rule::On,
                    };
                let refusal = self.enable_refusal(capability, network);
                let allowed = rule != Rule::Forbidden && (!available || effective || refusal.is_none());
                let blocked = choice.and_then(|choice| choice.blocked.as_deref());
                let reason = if let Some(blocked) = blocked {
                    format!("session choice blocked: {blocked}")
                } else if rule == Rule::Forbidden {
                    "forbidden by policy or session restrictions".into()
                } else if capability == "public_web" && network.mode == NetMode::None {
                    "network disabled by resolved policy".into()
                } else if !available {
                    unavailable_reason(capability)
                } else if effective && capability == "test_desktop" {
                    "an isolated Weston desktop with Xwayland; the real display stays out of reach".into()
                } else if effective && capability == "private_bus" {
                    "a D-Bus session bus of the jail's own; the person's buses stay out of reach".into()
                } else if effective && capability == "gpu" {
                    "GPU render node enabled; exposes GPU driver ioctls and shared GPU resources".into()
                } else if effective {
                    "public web proxy enabled; local destinations remain separately restricted".into()
                } else if let Some(refusal) = refusal {
                    refusal.into()
                } else {
                    "off; explicit permission required to widen access".into()
                };
                State {
                    capability,
                    allowed,
                    default_enabled: choice.map_or(rule == Rule::On, |choice| choice.policy_default),
                    requested: choice.map(|choice| choice.enabled),
                    blocked: blocked.is_some(),
                    source: if choice.is_some_and(|choice| choice.blocked.is_none()) { "session" } else { "policy" },
                    effective,
                    available,
                    reason,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    fn choices(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
        pairs.iter().map(|(name, enabled)| (name.to_string(), *enabled)).collect()
    }

    #[test]
    fn defaults_and_hard_limits_are_distinct() {
        let p = Policy::parse("name='test'\n[permissions]\nprivate_bus='off'\ngpu='forbidden'\npublic_web='on'").unwrap();
        let states = p.permissions.states(&p.network, &[]);
        // (the private bus's backend depends on the machine; off is allowed and not effective)
        assert!(states[1].allowed && !states[1].effective);
        assert!(!states[2].allowed && !states[2].available, "the GPU has no backend yet");
        assert!(states[3].effective);
        assert!(p.permissions.validate_launch(&p.network).is_ok());
    }

    #[test]
    fn host_authority_cannot_be_a_saved_default() {
        assert!(Policy::parse("name='bad'\n[permissions]\nhost_control='on'").is_err());
        assert!(Policy::parse("name='bad'\n[permissions]\nunknown='off'").is_err());
        assert!(Policy::parse("name='bad'\n[permissions]\npublic_web='forbidden'\n[network]\nmode='all'").is_err());
    }

    #[test]
    fn unsupported_defaults_never_claim_enforcement() {
        let p = Policy::parse("name='future'\n[permissions]\ngpu='on'").unwrap();
        let state = &p.permissions.states(&p.network, &[])[2];
        assert!(state.default_enabled);
        assert_eq!(state.effective, available("gpu"));
        assert_eq!(state.available, available("gpu"));
        assert_eq!(p.permissions.validate_launch(&p.network).is_ok(), available("gpu"));
        assert!(p.permissions.uses_modes());
    }

    #[test]
    fn offline_and_ghost_restrictions_win() {
        let mut p = Policy::parse("name='test'\n[network]\nmode='none'\n[permissions]\npublic_web='on'").unwrap();
        let state = &p.permissions.states(&p.network, &[])[3];
        assert!(!state.allowed && !state.effective);
        // Offline overrides the requested default without changing its provenance.
        assert!(p.permissions.validate_launch(&p.network).is_ok());
        p.ghostly(std::path::Path::new("/nonexistent"));
        assert!(p.permissions.states(&p.network, &[]).iter().all(|state| !state.allowed && !state.effective));
    }

    /// The test desktop is not a network grant: offline policies and the additional-hosts ceiling
    /// do not refuse it, and where this machine has Weston it is effective exactly when on.
    #[test]
    fn the_test_desktop_ignores_network_limits() {
        let p = Policy::parse("name='t'\n[network]\nmode='none'\nallow_additional_hosts=false").unwrap();
        let refusal = p.permissions.enable_refusal("test_desktop", &p.network);
        if available("test_desktop") {
            assert_eq!(refusal, None);
            let p = Policy::parse("name='t'\n[network]\nmode='none'\n[permissions]\ntest_desktop='on'").unwrap();
            let state = &p.permissions.states(&p.network, &[])[0];
            assert!(state.effective && state.available && state.reason.contains("Weston"));
            assert!(p.permissions.validate_launch(&p.network).is_ok());
        } else {
            assert_eq!(refusal, Some("no test desktop on this machine (Weston and Xwayland)"));
        }
    }

    #[test]
    fn all_mode_policies_report_public_web_as_their_default() {
        let p = Policy::parse("name='open'\n[network]\nmode='all'").unwrap();
        assert_eq!(p.permissions.public_web, Rule::On);
        assert!(p.permissions.states(&p.network, &[])[3].default_enabled);
    }

    #[test]
    fn session_choices_widen_only_within_limits_and_always_tighten() {
        let mut p = Policy::parse("name='test'\n[network]\nmode='allow'").unwrap();
        let applied = p.permissions.apply_session(&mut p.network, &choices(&[("public_web", true), ("gpu", true)]));
        assert_eq!(p.network.mode, NetMode::All);
        assert!(applied.iter().find(|c| c.capability == "public_web").unwrap().blocked.is_none());
        let gpu = applied.iter().find(|c| c.capability == "gpu").unwrap();
        assert_eq!(gpu.blocked.is_none(), available("gpu"));
        // A blocked desktop choice is reported, never forced into a launch it would refuse.
        assert!(p.permissions.validate_launch(&p.network).is_ok());
        assert_eq!(p.permissions.uses_modes(), available("gpu"));
        let states = p.permissions.states(&p.network, &applied);
        assert!(states[3].effective && states[3].source == "session" && states[3].requested == Some(true) && !states[3].default_enabled);
        assert_eq!(states[2].blocked, !available("gpu"));

        let mut p = Policy::parse("name='test'\n[permissions]\npublic_web='on'\ngpu='on'").unwrap();
        let applied = p.permissions.apply_session(&mut p.network, &choices(&[("public_web", false), ("gpu", false)]));
        assert_eq!(p.network.mode, NetMode::Allow);
        assert!(p.permissions.validate_launch(&p.network).is_ok(), "a session may decline a policy default");
        let states = p.permissions.states(&p.network, &applied);
        assert!(!states[3].effective && states[3].default_enabled && states[3].requested == Some(false));
        assert!(states[2].default_enabled && !p.permissions.uses_modes());
    }

    #[test]
    fn the_private_bus_depends_on_dbus_daemon() {
        let p = Policy::parse("name='t'\n[network]\nmode='none'\n[permissions]\nprivate_bus='on'").unwrap();
        let state = &p.permissions.states(&p.network, &[])[1];
        assert_eq!(state.capability, "private_bus");
        if available("private_bus") {
            assert!(p.permissions.enable_refusal("private_bus", &p.network).is_none(), "not a network grant");
            assert!(state.effective && state.reason.contains("jail's own"), "{}", state.reason);
            assert!(p.permissions.validate_launch(&p.network).is_ok());
        } else {
            assert!(p.permissions.enable_refusal("private_bus", &p.network).unwrap().contains("dbus-daemon"));
            assert!(!state.effective && state.reason.contains("dbus-daemon"), "{}", state.reason);
        }
    }

    #[test]
    fn session_choices_never_pass_forbidden_offline_ceiling_or_host_control() {
        for (text, reason) in [
            ("name='t'\n[permissions]\npublic_web='forbidden'", "forbidden"),
            ("name='t'\n[network]\nmode='none'", "network disabled"),
            ("name='t'\n[network]\nallow_additional_hosts=false", "additional hosts"),
        ] {
            let mut p = Policy::parse(text).unwrap();
            let before = p.network.mode;
            let applied = p.permissions.apply_session(&mut p.network, &choices(&[("public_web", true)]));
            assert!(applied[0].blocked.as_deref().unwrap().contains(reason), "{text}");
            assert_eq!(p.network.mode, before);
        }
        let mut p = Policy::parse("name='t'").unwrap();
        let applied = p.permissions.apply_session(&mut p.network, &choices(&[("host_control", true), ("unknown", true)]));
        assert!(applied.iter().all(|c| c.blocked.is_some()));
        assert_eq!(p.permissions.host_control, Rule::Off);
        p.ghostly(std::path::Path::new("/nonexistent"));
        let applied = p.permissions.apply_session(&mut p.network, &choices(&[("public_web", true)]));
        assert!(applied[0].blocked.is_some() && p.network.mode != NetMode::All);
    }
}
