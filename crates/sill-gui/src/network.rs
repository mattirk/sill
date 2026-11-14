//! Proxy denials are runtime records, independent of the agent conversation stream.
use crate::{App, Message, SessionView, blocking, json};
use iced::widget::{button, column, container, row, text};
use iced::{Element, Task};
use sill_client::actions::Outcome;
use sill_proto::admin::{DeniedList, NetworkDenied};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct State {
    reading: BTreeSet<String>,
    hosts: BTreeMap<String, Vec<String>>,
    pending: BTreeSet<(String, String)>,
    notices: BTreeMap<String, String>,
}

fn visible(data: &NetworkDenied) -> Vec<String> {
    data.hosts
        .iter()
        .filter(|host| {
            let name = host
                .rsplit_once(':')
                .filter(|(_, p)| p.parse::<u16>().is_ok())
                .map_or(host.as_str(), |(h, _)| h);
            !data.answered.contains(host)
                && !data.quiet.iter().any(|p| {
                    p.eq_ignore_ascii_case(name)
                        || p.strip_prefix("*.").is_some_and(|suffix| {
                            name.to_ascii_lowercase()
                                .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
                        })
                })
        })
        .take(8)
        .cloned()
        .collect()
}

impl State {
    pub fn read(&mut self, id: String, result: Result<NetworkDenied, String>) {
        self.reading.remove(&id);
        if let Ok(data) = result {
            self.hosts.insert(id, visible(&data));
        }
    }
    pub fn done(&mut self, id: String, host: String, outcome: Outcome) {
        self.pending.remove(&(id.clone(), host.clone()));
        if outcome.is_done() {
            if let Some(hosts) = self.hosts.get_mut(&id) {
                hosts.retain(|h| h != &host);
            }
        }
        self.notices.insert(
            id,
            outcome
                .problem()
                .unwrap_or("Network choice saved.")
                .to_string(),
        );
    }
}

impl App {
    pub(crate) fn poll_network(&mut self) -> Task<Message> {
        let (Some(id), Some(actions)) = (self.selected.clone(), self.actions.clone()) else {
            return Task::none();
        };
        if !self.network.pending.is_empty() || !self.network.reading.insert(id.clone()) {
            return Task::none();
        }
        let session = id.clone();
        blocking(
            move || {
                actions
                    .reading(|client| {
                        let mut all = NetworkDenied {
                            next: None,
                            hosts: vec![],
                            answered: vec![],
                            quiet: vec![],
                            agent_name: String::new(),
                        };
                        let mut after = None;
                        loop {
                            let page: NetworkDenied = client.call(
                                "_sill/policy.denied",
                                DeniedList {
                                    session_id: session.clone(),
                                    after,
                                    limit: Some(128),
                                },
                            )?;
                            all.hosts.extend(page.hosts);
                            all.answered.extend(page.answered);
                            all.quiet.extend(page.quiet);
                            all.agent_name = page.agent_name;
                            after = page.next;
                            if after.is_none() {
                                return Ok(all);
                            }
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            },
            move |result| Message::NetworkRead(id, result),
        )
    }

    pub(crate) fn answer_network(
        &mut self,
        id: String,
        host: String,
        allow: bool,
    ) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        if !self.network.pending.is_empty() {
            return Task::none();
        }
        if !self.network.pending.insert((id.clone(), host.clone())) {
            return Task::none();
        }
        let (session, target) = (id.clone(), host.clone());
        blocking(
            move || {
                let params = json!({"session_id": session, "host": target});
                if allow {
                    let result = actions.call("_sill/policy.allow_host", params.clone());
                    let Outcome::Done(value) = result else {
                        return result;
                    };
                    if value["agent_name"] != "this session" {
                        let key = json!({"session_id": session});
                        let stopped = actions.call("_sill/sessions.stop", key.clone());
                        if !stopped.is_done() {
                            return stopped;
                        }
                        let started = actions.call("_sill/sessions.start", key);
                        if !started.is_done() {
                            return started;
                        }
                    }
                }
                actions.call("_sill/policy.remember_denied", params)
            },
            move |result| Message::NetworkDone(id, host, result),
        )
    }

    pub(crate) fn network_prompt<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let mut content = column![].spacing(6);
        if let Some(hosts) = self.network.hosts.get(&view.id) {
            for host in hosts {
                let pending = !self.network.pending.is_empty();
                let allow = button(text(format!("Allow {host}"))).on_press_maybe(
                    (!pending).then(|| Message::NetworkAnswer(view.id.clone(), host.clone(), true)),
                );
                let block = button("Keep blocked").on_press_maybe(
                    (!pending)
                        .then(|| Message::NetworkAnswer(view.id.clone(), host.clone(), false)),
                );
                content = content.push(text(format!("Proxy blocked {host}. Allow saves access for this agent and restarts this session."))).push(row![allow, block].spacing(8));
            }
        }
        if let Some(note) = self.network.notices.get(&view.id) {
            content = content.push(text(note));
        }
        container(content).padding([0, 12]).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_choice_stays_visible_and_success_removes_only_chosen_host() {
        let mut state = State::default();
        state
            .hosts
            .insert("s".into(), vec!["a.com".into(), "b.com".into()]);
        state.pending.insert(("s".into(), "a.com".into()));
        state.done(
            "s".into(),
            "a.com".into(),
            Outcome::Uncertain("connection lost".into()),
        );
        assert!(state.pending.is_empty());
        assert_eq!(state.hosts["s"].len(), 2);
        assert_eq!(state.notices["s"], "connection lost");
        state.done("s".into(), "a.com".into(), Outcome::Done(json!({})));
        assert_eq!(state.hosts["s"], vec!["b.com"]);
    }
    #[test]
    fn prompts_exclude_answered_and_quiet_hosts_and_are_bounded() {
        let mut data = NetworkDenied {
            next: None,
            hosts: vec![
                "terminalcolors.com".into(),
                "answered.com".into(),
                "metrics.example.com:443".into(),
            ],
            answered: vec!["answered.com".into()],
            quiet: vec!["*.example.com".into()],
            agent_name: "Claude".into(),
        };
        assert_eq!(visible(&data), vec!["terminalcolors.com"]);
        data.hosts.extend((0..20).map(|n| format!("host{n}.com")));
        assert_eq!(visible(&data).len(), 8);
    }
}
