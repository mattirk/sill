//! Policy capability defaults and ceilings. Host authority is always an explicit activation.
use crate::policy::{NetMode, Network};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

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

pub struct State {
    pub capability: &'static str,
    pub allowed: bool,
    pub default_enabled: bool,
    pub effective: bool,
    pub available: bool,
    pub reason: &'static str,
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

    pub fn validate(&self, network: &Network) -> Result<()> {
        ensure!(
            self.host_control != Rule::On,
            "Host control cannot be enabled by a policy default; explicit activation is required"
        );
        ensure!(
            self.public_web != Rule::Forbidden || network.mode != NetMode::All,
            "public_web is forbidden but network.mode is all"
        );
        Ok(())
    }

    pub fn apply_defaults(&self, network: &mut Network) {
        if self.public_web == Rule::On && network.mode != NetMode::None {
            network.mode = NetMode::All;
        }
    }

    pub fn validate_launch(&self, network: &Network) -> Result<()> {
        self.validate(network)?;
        for (name, rule) in self.rules() {
            ensure!(
                name == "public_web" || rule != Rule::On,
                "{name} was requested but its enforcement backend is not implemented"
            );
        }
        Ok(())
    }

    pub fn states(&self, network: &Network) -> Vec<State> {
        self.rules()
            .into_iter()
            .map(|(capability, rule)| {
                let available = capability == "public_web";
                let effective = available && network.mode == NetMode::All;
                let allowed =
                    rule != Rule::Forbidden && (!available || network.mode != NetMode::None);
                let reason = if rule == Rule::Forbidden {
                    "forbidden by policy or session restrictions"
                } else if available && network.mode == NetMode::None {
                    "network disabled by resolved policy"
                } else if !available {
                    "enforcement backend not implemented"
                } else if effective {
                    "public web proxy enabled; local destinations remain separately restricted"
                } else {
                    "off; explicit permission required to widen access"
                };
                State {
                    capability,
                    allowed,
                    default_enabled: rule == Rule::On,
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

    #[test]
    fn defaults_and_hard_limits_are_distinct() {
        let p = Policy::parse(
            "name='test'\n[permissions]\ntest_desktop='off'\ngpu='forbidden'\npublic_web='on'",
        )
        .unwrap();
        let states = p.permissions.states(&p.network);
        assert!(states[0].allowed && !states[0].effective && !states[0].available);
        assert!(!states[2].allowed);
        assert!(states[3].effective);
        assert!(p.permissions.validate_launch(&p.network).is_ok());
    }

    #[test]
    fn host_authority_cannot_be_a_saved_default() {
        assert!(Policy::parse("name='bad'\n[permissions]\nhost_control='on'").is_err());
        assert!(Policy::parse("name='bad'\n[permissions]\nunknown='off'").is_err());
        assert!(
            Policy::parse(
                "name='bad'\n[permissions]\npublic_web='forbidden'\n[network]\nmode='all'"
            )
            .is_err()
        );
    }

    #[test]
    fn unsupported_defaults_never_claim_enforcement() {
        let p = Policy::parse("name='future'\n[permissions]\ntest_desktop='on'").unwrap();
        let state = &p.permissions.states(&p.network)[0];
        assert!(state.default_enabled && !state.effective && !state.available);
        assert!(p.permissions.validate_launch(&p.network).is_err());
    }

    #[test]
    fn repository_offline_and_ghost_restrictions_win() {
        let mut p = Policy::parse("name='test'\n[permissions]\npublic_web='on'").unwrap();
        p.network.mode = NetMode::None;
        let state = &p.permissions.states(&p.network)[3];
        assert!(!state.allowed && !state.effective);
        // Repository restrictions override the requested default without changing its provenance.
        assert!(p.permissions.validate_launch(&p.network).is_ok());
        p.ghostly(std::path::Path::new("/nonexistent"));
        assert!(
            p.permissions
                .states(&p.network)
                .iter()
                .all(|state| !state.allowed && !state.effective)
        );
    }
}
