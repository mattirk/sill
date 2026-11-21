//! Proxy denials are runtime records, independent of the agent conversation stream. A card over
//! the composer per refused host: Allow adds
//! it to the agent's allow-list and, unless it was for this session only, starts the session
//! again and attaches anew, which brings the agent back up; Keep blocked remembers the answer.
//! What came of it is a short note that goes by itself.
use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, json, keys};
use iced::widget::{Column, button, container, row, space, text};
use iced::{Element, Fill, Task};
use silli_client::actions::Outcome;
use silli_proto::admin::{DeniedList, NetworkDenied};
use silli_proto::method;
use std::collections::{BTreeMap, BTreeSet};

/// How long a note on a network choice stays.
const NOTICE: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 0 } else { 6 });

#[derive(Default)]
pub(crate) struct State {
    reading: BTreeSet<String>,
    hosts: BTreeMap<String, Vec<String>>,
    /// The agent each session runs, as the daemon names it in its answers.
    agents: BTreeMap<String, String>,
    pending: BTreeSet<(String, String)>,
    /// A session's note on its last choice: its number (a later one outlives an earlier one's
    /// expiry), the text, and whether it is a failure.
    notices: BTreeMap<String, (u64, String, bool)>,
    notice_serial: u64,
}

/// What came of a network choice, for its note.
#[derive(Debug, Clone)]
pub struct Choice {
    pub host: String,
    pub allow: bool,
    pub quiet: bool,
    /// The session was started again (an allow for the agent, not this session alone).
    pub restarted: bool,
}

fn visible(data: &NetworkDenied) -> Vec<String> {
    data.hosts
        .iter()
        .filter(|host| {
            let name = host.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map_or(host.as_str(), |(h, _)| h);
            !data.answered.contains(host)
                && !data.quiet.iter().any(|p| {
                    p.eq_ignore_ascii_case(name)
                        || p.strip_prefix("*.").is_some_and(|suffix| name.to_ascii_lowercase().ends_with(&format!(".{}", suffix.to_ascii_lowercase())))
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
            if !data.agent_name.is_empty() {
                self.agents.insert(id.clone(), data.agent_name.clone());
            }
            self.hosts.insert(id, visible(&data));
        }
    }

    /// The choice is made (or failed): its note, numbered for its expiry.
    pub fn done(&mut self, id: String, choice: &Choice, outcome: &Outcome) -> u64 {
        let host = &choice.host;
        self.pending.remove(&(id.clone(), host.clone()));
        if outcome.is_done()
            && let Some(hosts) = self.hosts.get_mut(&id)
        {
            hosts.retain(|h| h != host);
        }
        let agent = self.agents.get(&id).map_or("the agent", String::as_str);
        let (said, failed) = match outcome.problem() {
            Some(problem) => (format!("{host}: {problem}"), true),
            None if choice.quiet => (format!("{host}: won’t ask again in any session. Network access is unchanged."), false),
            None if !choice.allow => (format!("{host} stays blocked."), false),
            None if choice.restarted => (format!("{host} allowed for {agent}; the session starts again."), false),
            None => (format!("{host} allowed for this session."), false),
        };
        self.notice_serial += 1;
        self.notices.insert(id, (self.notice_serial, said, failed));
        self.notice_serial
    }

    /// A note's time is up, unless a later one took its place.
    pub fn expire(&mut self, id: &str, serial: u64) {
        if self.notices.get(id).is_some_and(|(n, _, _)| *n == serial) {
            self.notices.remove(id);
        }
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
                        let mut all = NetworkDenied { next: None, hosts: vec![], answered: vec![], quiet: vec![], agent_name: String::new() };
                        let mut after = None;
                        loop {
                            let page: NetworkDenied =
                                client.call(method::POLICY_DENIED, DeniedList { session_id: session.clone(), after, limit: Some(128) })?;
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

    pub(crate) fn answer_network(&mut self, id: String, host: String, allow: bool, quiet: bool) -> Task<Message> {
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
                let mut restarted = false;
                if quiet {
                    return (actions.call_needing("silli.network_quiet", method::POLICY_QUIET_HOST, params), false);
                }
                if allow {
                    let result = actions.call(method::POLICY_ALLOW_HOST, params.clone());
                    let Outcome::Done(value) = result else {
                        return (result, restarted);
                    };
                    // a live grant reaches the running proxy; only an older daemon (or a
                    // wildcard) needs the jail rebuilt
                    let live = value["live"].as_bool().unwrap_or(false);
                    if value["agent_name"] != "this session" && !live {
                        let key = json!({"session_id": session});
                        let stopped = actions.call(method::SESSIONS_STOP, key.clone());
                        if !stopped.is_done() {
                            return (stopped, restarted);
                        }
                        let started = actions.call(method::SESSIONS_START, key);
                        if !started.is_done() {
                            return (started, restarted);
                        }
                        restarted = true;
                    }
                }
                (actions.call(method::POLICY_REMEMBER_DENIED, params), restarted)
            },
            move |(outcome, restarted)| Message::NetworkDone(id, Choice { host, allow, quiet, restarted }, outcome),
        )
    }

    /// A choice's outcome: its note, gone after a while; and after a restart the session is
    /// attached anew, which starts its agent (a restart alone leaves it down until a prompt).
    pub(crate) fn network_done(&mut self, id: String, choice: Choice, outcome: Outcome) -> Task<Message> {
        let serial = self.network.done(id.clone(), &choice, &outcome);
        if choice.restarted {
            self.reattach(&id);
        }
        let session = id.clone();
        let expire = blocking(move || std::thread::sleep(NOTICE), move |()| Message::NetworkNoticeExpired(session, serial));
        // the agent usually ended its turn on the refusal: it is told to go on, so the work
        // continues without the person asking again (a busy agent sees the host work next time)
        let idle = self.views.get(&id).is_some_and(|v| !v.conversation.busy());
        if choice.allow && outcome.is_done() && (idle || choice.restarted) {
            return Task::batch([expire, self.carry_on(&id, &choice.host)]);
        }
        expire
    }

    /// A marked prompt telling the agent the host is reachable now.
    fn carry_on(&self, id: &str, host: &str) -> Task<Message> {
        let host = host.strip_suffix(":443").unwrap_or(host);
        let submit = silli_proto::sfp::Submit {
            session_id: id.to_string(),
            submission_id: silli_client::request_id("carry-on"),
            text: format!("[silli: the person allowed network access to {host}. Carry on with what needed it.]"),
            attachments: vec![],
            purpose: None,
        };
        self.act(id, crate::Act::CarryOn, method::SESSIONS_SUBMIT, json!(submit))
    }

    /// A card per refused host, then the note on the last choice; nothing at all otherwise.
    pub(crate) fn network_prompt<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let hosts = self.network.hosts.get(&view.id).filter(|h| !h.is_empty());
        let notice = self.network.notices.get(&view.id);
        if hosts.is_none() && notice.is_none() {
            return space().height(0).into();
        }
        let agent = self.network.agents.get(&view.id).map_or("The agent", String::as_str);
        let pending = !self.network.pending.is_empty();
        let mut content = Column::new().spacing(6);
        for host in hosts.into_iter().flatten() {
            // the HTTPS port goes without saying; another port is part of the name
            let shown = host.strip_suffix(":443").unwrap_or(host);
            let answer = |allow| (!pending).then(|| Message::NetworkAnswer(view.id.clone(), host.clone(), allow, false));
            let card = Column::new()
                .spacing(8)
                .push(text(format!("{agent} tried to reach {shown} and the proxy refused it: the host is not on this agent's allow-list.")).size(style::SMALL))
                .push(
                    row![
                        keys::press_maybe(
                            button(text(format!("Allow {shown}")).size(style::SMALL)).padding([4, 10]).style(style::solid(P.accent)),
                            answer(true),
                        ),
                        keys::press_maybe(button(text("Keep blocked").size(style::SMALL)).padding([4, 10]).style(style::quiet), answer(false),),
                        keys::press_maybe(
                            button(text("Don’t ask again").size(style::SMALL)).padding([4, 10]).style(style::quiet),
                            (!pending).then(|| Message::NetworkAnswer(view.id.clone(), host.clone(), false, true,)),
                        ),
                    ]
                    .spacing(8),
                );
            content =
                content.push(crate::ui::ruled(2.0, P.warning, container(card).padding([6, 10]).width(Fill).style(style::card_right(alpha(P.warning, 0.10)))));
        }
        if let Some((_, note, failed)) = notice {
            content = content.push(text(note).size(style::TINY).color(if *failed { P.error } else { alpha(P.fg, 0.7) }));
        }
        container(content).padding([6, 12]).width(Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_choice_stays_visible_and_success_removes_only_chosen_host() {
        let mut state = State::default();
        state.hosts.insert("s".into(), vec!["a.com".into(), "b.com".into()]);
        state.pending.insert(("s".into(), "a.com".into()));
        let allow = Choice { host: "a.com".into(), allow: true, quiet: false, restarted: true };
        state.done("s".into(), &allow, &Outcome::Uncertain("connection lost".into()));
        assert!(state.pending.is_empty());
        assert_eq!(state.hosts["s"].len(), 2);
        assert_eq!(state.notices["s"].1, "a.com: connection lost");
        assert!(state.notices["s"].2, "a failure");
        let serial = state.done("s".into(), &allow, &Outcome::Done(json!({})));
        assert_eq!(state.hosts["s"], vec!["b.com"]);
        assert_eq!(state.notices["s"].1, "a.com allowed for the agent; the session starts again.");
        // a note goes after its time, but not one that came after it
        state.expire("s", serial - 1);
        assert!(state.notices.contains_key("s"));
        state.expire("s", serial);
        assert!(!state.notices.contains_key("s"));
    }
    /// An allow the daemon grants live needs no restart, and an agent that ended its turn on the
    /// refusal is told to carry on: the work goes on without the person asking again.
    #[test]
    fn a_live_allow_keeps_the_session_and_the_agent_carries_on() {
        use crate::tests::{connector, daemon, settle_task};
        let (client, mut daemon) = daemon();
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        app.actions = Some(silli_client::actions::Actions::new(connector(vec![client])));
        // the agent ended its turn on the refusal
        let session = app.views.get_mut("demo").unwrap().conversation.session.as_mut().unwrap();
        session.activity = "idle".into();
        session.turn_id = None;
        let fake = std::thread::spawn(move || {
            let mut seen = vec![];
            let allow = daemon.expect(method::POLICY_ALLOW_HOST);
            daemon.reply(&allow, json!({"agent_name": "Codex", "live": true}));
            seen.push("allow".to_string());
            let remember = daemon.expect(method::POLICY_REMEMBER_DENIED);
            daemon.reply(&remember, json!({}));
            let submit = daemon.expect(method::SESSIONS_SUBMIT);
            seen.push(submit["params"]["text"].as_str().unwrap().to_string());
            daemon.reply(&submit, json!({}));
            seen
        });
        let task = app.update(Message::NetworkAnswer("demo".into(), "example.com:443".into(), true, false));
        settle_task(&mut app, task);
        let seen = fake.join().unwrap();
        assert_eq!(
            seen[1], "[silli: the person allowed network access to example.com. Carry on with what needed it.]",
            "no stop or start came between: {seen:?}"
        );
    }

    #[test]
    fn prompts_exclude_answered_and_quiet_hosts_and_are_bounded() {
        let mut data = NetworkDenied {
            next: None,
            hosts: vec!["terminalcolors.com".into(), "answered.com".into(), "metrics.example.com:443".into()],
            answered: vec!["answered.com".into()],
            quiet: vec!["*.example.com".into()],
            agent_name: "Claude".into(),
        };
        assert_eq!(visible(&data), vec!["terminalcolors.com"]);
        data.hosts.extend((0..20).map(|n| format!("host{n}.com")));
        assert_eq!(visible(&data).len(), 8);
    }
}
