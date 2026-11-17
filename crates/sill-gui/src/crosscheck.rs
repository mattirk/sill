//! Crosscheck in the window (docs/crosscheck.md): one line under a workspace's line on the shelf
//! above the prompt, and the Reviewer in Settings. The line says who reviews and what they found;
//! it opens to the Reviewer's steps while it works and to its summary and problems after. The
//! cross at its right end is the one way out at every stage. The review itself is the daemon's
//! (`sill_client::crosscheck`), read off the GUI thread.
use std::collections::{BTreeMap, BTreeSet};

use iced::widget::{Column, button, column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Fill, Padding, Task};
use sill_client::crosscheck::{self as cc, Stage};
use sill_proto::crosscheck as api;
use sill_proto::sfp::{ControlOption, ControlValue};
use sill_proto::workspaces::Detail;

use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, icons, keys};

/// The steps a working Reviewer's line opens to: this many, the latest last. The list keeps this
/// height from the first step on, and older steps leave at the top.
const STEPS: usize = 6;

/// Every workspace's review as the window knows it, and the Reviewer setting.
#[derive(Debug, Default)]
pub struct State {
    /// Whether the daemon does Crosscheck: the button is offered only then.
    pub offered: bool,
    /// Workspace id → its review as last read.
    pub runs: BTreeMap<String, api::Run>,
    /// The workspaces whose line is open.
    pub open: BTreeSet<String>,
    /// The workspaces whose start has been sent and not yet answered.
    pub starting: BTreeSet<String>,
    /// Reviews the person removed: an answer read before the removal does not bring one back.
    removed: BTreeSet<String>,
    /// Why a workspace's review did not start, or was not removed.
    pub said: BTreeMap<String, String>,
    /// The Reviewer setting as last read, for Settings.
    pub reviewers: Option<Result<api::Reviewers, String>>,
}

impl State {
    /// A Reviewer is at work, or a start is on its way: the line is read again every two seconds.
    pub fn ticking(&self) -> bool {
        !self.starting.is_empty() || self.runs.values().any(|r| cc::stage(r) == Stage::Working)
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Read again what is under way.
    Poll,
    Loaded(String, Result<cc::Seen, String>),
    /// The Crosscheck button, Review again, Try again: (workspace).
    Start(String),
    Started(String, Result<api::Run, String>),
    /// The cross: cancel a review under way, remove a finished one.
    Remove(String),
    Removed(String, Result<(), String>),
    /// Open or close a workspace's line.
    Toggle(String),
    /// Ask the session's agent to fix the problems: (session, workspace).
    Fix(String, String),
    /// Settings: the Reviewer setting was read or saved.
    Reviewers(Result<api::Reviewers, String>),
    /// Settings: the Reviewer's agent; None is "Automatic".
    Agent(Option<String>),
    /// Settings: one of the Reviewer's options, (option id, value).
    Choose(String, String),
}

fn message(e: &anyhow::Error) -> String {
    sill_client::workspaces::message(e)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// One entry of a Settings dropdown here: what it says and what it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pick {
    label: String,
    value: Option<String>,
}

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn dropdown<'a>(picks: Vec<Pick>, current: Option<Pick>, on: impl Fn(Pick) -> Message + 'a) -> Element<'a, Message> {
    pick_list(picks, current, on).text_size(style::TEXT).padding([6, 12]).style(crate::settings_ui::dropdown).menu_style(style::control_menu).into()
}

/// The value an option has for the Reviewer: what was chosen for it, else what the agent starts with.
fn chosen(reviewer: Option<&api::Reviewer>, option: &ControlOption) -> String {
    reviewer.and_then(|r| r.options.get(&option.option_id).cloned()).unwrap_or_else(|| match &option.value {
        ControlValue::Id { value } => value.clone(),
        ControlValue::Boolean { value } => value.to_string(),
    })
}

fn small_button<'a>(label: impl Into<String>, outlined: bool, on: Option<Message>) -> Element<'a, Message> {
    let style = if outlined { style::outlined as fn(&iced::Theme, button::Status) -> button::Style } else { style::quiet };
    keys::press_maybe(button(text(label.into()).size(style::SMALL)).padding([3, 8]).style(style), on)
}

impl App {
    /// Read a workspace's review.
    pub(crate) fn crosscheck_load(&self, workspace: String) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        blocking(
            {
                let workspace = workspace.clone();
                move || cc::get(&actions, &workspace).map_err(|e| message(&e))
            },
            move |seen| Message::Crosscheck(Event::Loaded(workspace, seen)),
        )
    }

    /// Read the Reviewer setting, for Settings.
    pub(crate) fn crosscheck_reviewers_load(&self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        blocking(move || cc::reviewers(&actions).map_err(|e| message(&e)), |r| Message::Crosscheck(Event::Reviewers(r)))
    }

    fn crosscheck_save(&mut self, reviewers: Vec<api::Reviewer>) -> Task<Message> {
        // shown at once; the daemon's answer is what stays
        if let Some(Ok(setup)) = &mut self.crosscheck.reviewers {
            setup.reviewers = reviewers.clone();
        }
        let Some(actions) = self.actions.clone() else { return Task::none() };
        blocking(move || cc::set_reviewers(&actions, &reviewers).map_err(|e| message(&e)), |r| Message::Crosscheck(Event::Reviewers(r)))
    }

    pub(crate) fn crosscheck_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Poll => {
                let working: Vec<String> = self.crosscheck.runs.iter().filter(|(_, r)| cc::stage(r) == Stage::Working).map(|(w, _)| w.clone()).collect();
                Task::batch(working.into_iter().map(|w| self.crosscheck_load(w)))
            }
            Event::Loaded(workspace, Ok(seen)) => {
                self.crosscheck.offered = seen.offered;
                match seen.run.filter(|r| !self.crosscheck.removed.contains(&r.run_id)) {
                    Some(run) => {
                        self.crosscheck.runs.insert(workspace, run);
                    }
                    // a start on its way is not yet the daemon's to report
                    None if self.crosscheck.starting.contains(&workspace) => {}
                    None => {
                        self.crosscheck.runs.remove(&workspace);
                        self.crosscheck.open.remove(&workspace);
                    }
                }
                Task::none()
            }
            // the line stays as it was read last; the next read says
            Event::Loaded(_, Err(_)) => Task::none(),
            Event::Start(workspace) => {
                let Some(actions) = self.actions.clone() else { return Task::none() };
                if !self.crosscheck.starting.insert(workspace.clone()) {
                    return Task::none();
                }
                self.crosscheck.said.remove(&workspace);
                self.crosscheck.open.remove(&workspace);
                let request = api::Start { operation_id: sill_client::request_id("crosscheck"), workspace_id: workspace.clone() };
                blocking(move || cc::start(&actions, &request).map_err(|e| message(&e)), move |run| Message::Crosscheck(Event::Started(workspace, run)))
            }
            Event::Started(workspace, started) => {
                self.crosscheck.starting.remove(&workspace);
                match started {
                    Ok(run) => {
                        self.crosscheck.runs.insert(workspace, run);
                    }
                    Err(why) => {
                        self.crosscheck.said.insert(workspace.clone(), why);
                        // what the daemon has for it now, an earlier review included
                        return self.crosscheck_load(workspace);
                    }
                }
                Task::none()
            }
            Event::Remove(workspace) => {
                self.crosscheck.open.remove(&workspace);
                self.crosscheck.said.remove(&workspace);
                // gone from the line at once; the daemon stops the Reviewer behind it
                let Some(run) = self.crosscheck.runs.remove(&workspace) else { return Task::none() };
                self.crosscheck.removed.insert(run.run_id.clone());
                let Some(actions) = self.actions.clone() else { return Task::none() };
                blocking(move || cc::remove(&actions, &run.run_id).map_err(|e| message(&e)), move |done| Message::Crosscheck(Event::Removed(workspace, done)))
            }
            Event::Removed(workspace, done) => {
                if let Err(why) = done {
                    self.crosscheck.said.insert(workspace.clone(), why);
                    self.crosscheck.removed.clear();
                    return self.crosscheck_load(workspace);
                }
                Task::none()
            }
            Event::Toggle(workspace) => {
                if !self.crosscheck.open.remove(&workspace) {
                    self.crosscheck.open.insert(workspace);
                }
                Task::none()
            }
            Event::Fix(session, workspace) => {
                let Some(run) = self.crosscheck.runs.get(&workspace).filter(|r| cc::stage(r) == Stage::Done && !r.problems.is_empty()) else {
                    return Task::none();
                };
                if self.views.get(&session).is_none_or(|v| v.conversation.busy()) {
                    return Task::none();
                }
                let request = api::fix_request(run);
                self.crosscheck.open.remove(&workspace);
                // the conversation shows the request as its first line, the rest folded (`folds`)
                self.submit_for(&session, request, vec![], Some(api::FIX_PURPOSE.into()))
            }
            Event::Reviewers(read) => {
                self.crosscheck.reviewers = Some(read);
                Task::none()
            }
            Event::Agent(agent) => {
                let Some(Ok(setup)) = &self.crosscheck.reviewers else { return Task::none() };
                // Another agent has other options. The Reviewer takes that agent's as they are
                // shown now, all of them, so that what Settings shows is what a review uses.
                let reviewers = match agent {
                    Some(agent) if setup.reviewers.first().is_some_and(|r| r.agent == agent) => return Task::none(),
                    Some(agent) => {
                        let mut rest = setup.reviewers.clone();
                        let options = setup
                            .agents
                            .iter()
                            .find(|a| a.agent == agent)
                            .map(|a| a.options.iter().map(|o| (o.option_id.clone(), chosen(None, o))).collect())
                            .unwrap_or_default();
                        let first = api::Reviewer { name: String::new(), agent, options };
                        if rest.is_empty() {
                            rest.push(first)
                        } else {
                            rest[0] = first
                        }
                        rest
                    }
                    None => setup.reviewers.iter().skip(1).cloned().collect(),
                };
                self.crosscheck_save(reviewers)
            }
            Event::Choose(option, value) => {
                let Some(Ok(setup)) = &self.crosscheck.reviewers else { return Task::none() };
                let mut reviewers = setup.reviewers.clone();
                let Some(first) = reviewers.first_mut() else { return Task::none() };
                first.options.insert(option, value);
                self.crosscheck_save(reviewers)
            }
        }
    }

    /// The Crosscheck button for a workspace's line: offered while it has no review.
    pub(crate) fn crosscheck_button<'a>(&'a self, d: &Detail) -> Option<Element<'a, Message>> {
        let id = &d.workspace.workspace_id;
        if !self.crosscheck.offered || self.crosscheck.runs.contains_key(id) {
            return None;
        }
        let starting = self.crosscheck.starting.contains(id);
        Some(small_button("Crosscheck", true, (!starting).then(|| Message::Crosscheck(Event::Start(id.clone())))))
    }

    /// What goes under a workspace's line: its review's line, what the line opens to, and why a
    /// start did not happen.
    pub(crate) fn crosscheck_rows<'a>(&'a self, view: &'a SessionView, d: &Detail) -> Vec<Element<'a, Message>> {
        let id = d.workspace.workspace_id.clone();
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        if let Some(run) = self.crosscheck.runs.get(&id) {
            let open = self.crosscheck.open.contains(&id);
            rows.push(self.crosscheck_line(&id, run, open));
            if open {
                rows.push(match cc::stage(run) {
                    Stage::Working => steps(run),
                    Stage::Done | Stage::Stale => self.crosscheck_report(view, &id, run),
                    Stage::Failed => failure(run),
                });
            }
        }
        if let Some(why) = self.crosscheck.said.get(&id) {
            rows.push(text(why).size(style::TINY).color(P.error).width(Fill).wrapping(text::Wrapping::WordOrGlyph).into());
        }
        rows
    }

    /// The line: a chevron, the stage's mark, who and what, then what can be done and the cross.
    fn crosscheck_line<'a>(&'a self, id: &str, run: &'a api::Run, open: bool) -> Element<'a, Message> {
        let stage = cc::stage(run);
        let color = match stage {
            Stage::Working => P.fg,
            Stage::Done if run.problems.is_empty() => P.success,
            Stage::Done => P.warning,
            Stage::Stale => P.fg_dim,
            Stage::Failed => P.error,
        };
        let mark: Element<'a, Message> = match stage {
            Stage::Working => crate::ui::spinner(P.accent),
            Stage::Done if run.problems.is_empty() => container(icons::glyph("check", 13.0, P.success)).center_x(15).into(),
            _ => crate::ui::dot(color),
        };
        // what is worth saying beside the verdict, in a few dim words
        let aside = match stage {
            Stage::Working => {
                run.reviewers.first().and_then(|p| p.activity.last()).cloned().or_else(|| (run.state == "capturing").then(|| "reading the work".to_string()))
            }
            Stage::Stale => Some("the work has changed since".to_string()),
            Stage::Failed => Some(run.detail.clone()).filter(|d| !d.trim().is_empty()),
            Stage::Done => None,
        };
        let mut said = row![
            icons::glyph(if open { "chevron-down" } else { "chevron-right" }, 11.0, alpha(P.fg, 0.5)),
            mark,
            text(format!("{} {}", cc::who(run), cc::verdict(run))).size(style::SMALL).color(color).wrapping(text::Wrapping::None),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if let Some(aside) = aside {
            said = said.push(text(format!("·  {}", crate::ui::one_line(&aside, 90))).size(style::SMALL).color(P.fg_dim).wrapping(text::Wrapping::None));
        }
        let toggle = keys::press(
            button(container(said).width(Fill).clip(true)).padding([3, 0]).width(Fill).style(style::row_button),
            Message::Crosscheck(Event::Toggle(id.to_string())),
        );
        let start = |label: &'static str, outlined: bool| {
            small_button(label, outlined, (!self.crosscheck.starting.contains(id)).then(|| Message::Crosscheck(Event::Start(id.to_string()))))
        };
        let mut line = row![toggle].spacing(6).align_y(Alignment::Center);
        line = match stage {
            Stage::Working => line.push(text(cc::elapsed(run, now_secs())).size(style::TINY).color(P.fg_dim)),
            Stage::Done => line.push(start("Review again", false)),
            Stage::Stale => line.push(start("Review again", true)),
            Stage::Failed => {
                let line = line.push(start("Try again", true));
                if self.vm_shown() && crate::vm_ui::storage_error(&run.detail) {
                    line.push(crate::settings_ui::quiet_button("Open VM storage", Some(Message::Vm(crate::vm_ui::Event::OpenStorage))))
                } else {
                    line
                }
            }
        };
        // the one way out, as plain as the cross that closes a session's tab
        line.push(keys::press(
            button(icons::glyph("x", 12.0, alpha(P.fg, 0.55))).padding([4, 6]).style(style::quiet),
            Message::Crosscheck(Event::Remove(id.to_string())),
        ))
        .into()
    }

    /// A finished review opened: the Reviewer's summary, its problems, and the one action.
    fn crosscheck_report<'a>(&'a self, view: &'a SessionView, id: &str, run: &'a api::Run) -> Element<'a, Message> {
        let mut body = Column::new().spacing(8);
        if let Some(summary) = run.reviewers.first().map(|p| p.summary.as_str()).filter(|s| !s.is_empty()) {
            body = body.push(text(summary).size(style::SMALL).color(P.fg).width(Fill).wrapping(text::Wrapping::WordOrGlyph));
        }
        let stale = cc::stage(run) == Stage::Stale;
        for (n, problem) in run.problems.iter().enumerate() {
            let mut words = column![text(problem.title.as_str()).size(style::SMALL).color(P.fg).width(Fill).wrapping(text::Wrapping::WordOrGlyph)].spacing(1);
            if let Some(path) = &problem.path {
                let place = problem.line.map_or_else(|| path.clone(), |line| format!("{path}:{line}"));
                words = words.push(text(place).size(style::TINY).font(crate::ui::MONO_FONT).color(P.fg_dim).wrapping(text::Wrapping::WordOrGlyph));
            }
            body = body.push(
                row![text((n + 1).to_string()).size(style::SMALL).color(if stale { P.fg_dim } else { P.warning }).width(18), words].align_y(Alignment::Start),
            );
        }
        if !run.problems.is_empty() && !stale {
            let agent = self.tabs.iter().find(|t| t.id == view.id).map_or("the agent", |t| t.agent.as_str());
            let busy = view.conversation.busy();
            body = body.push(
                row![
                    keys::press_maybe(
                        button(text(format!("Ask {agent} to fix these")).size(style::SMALL)).padding([3, 8]).style(style::outlined_accent),
                        (!busy).then(|| Message::Crosscheck(Event::Fix(view.id.clone(), id.to_string()))),
                    ),
                    text(match run.problems.len() {
                        1 => format!("Sends it to {agent} as your next message."),
                        2 => format!("Sends both to {agent} as your next message."),
                        _ => format!("Sends them to {agent} as your next message."),
                    })
                    .size(style::TINY)
                    .color(P.fg_dim),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        container(body).padding(Padding { top: 2.0, right: 10.0, bottom: 6.0, left: 24.0 }).width(Fill).into()
    }

    /// Settings → Agent behaviour → Crosscheck: the Reviewer's agent and that agent's options.
    pub(crate) fn crosscheck_settings<'a>(&'a self) -> Vec<Element<'a, Message>> {
        use crate::settings_ui::setting;
        let setup = match &self.crosscheck.reviewers {
            Some(Ok(setup)) => setup,
            Some(Err(why)) => return vec![setting("Reviewer", Some(why.as_str()), space())],
            None => return vec![setting("Reviewer", Some("The agent that reviews when you press Crosscheck"), text("…").size(style::TEXT).color(P.fg_dim))],
        };
        let reviewer = setup.reviewers.first();
        let automatic = Pick { label: "Automatic".into(), value: None };
        let mut picks = vec![automatic.clone()];
        picks.extend(setup.agents.iter().map(|a| Pick { label: a.agent.clone(), value: Some(a.agent.clone()) }));
        // a saved agent that is no longer set up is still what is saved
        if let Some(r) = reviewer.filter(|r| !setup.agents.iter().any(|a| a.agent == r.agent)) {
            picks.push(Pick { label: r.agent.clone(), value: Some(r.agent.clone()) });
        }
        let current = match reviewer {
            Some(r) => picks.iter().find(|p| p.value.as_deref() == Some(r.agent.as_str())).cloned(),
            None => Some(automatic),
        };
        let mut rows = vec![setting(
            "Reviewer",
            Some("The agent that reviews when you press Crosscheck"),
            dropdown(picks, current, |pick| Message::Crosscheck(Event::Agent(pick.value))),
        )];
        let Some(reviewer) = reviewer else {
            rows.push(note("Automatic: another agent than the one whose work it is, the one you used last. It reviews as it starts."));
            return rows;
        };
        let options = setup.agents.iter().find(|a| a.agent == reviewer.agent).map(|a| a.options.as_slice());
        match options {
            None => rows.push(note(format!("{} is not set up here any more. A review will say so; choose another Reviewer.", reviewer.agent))),
            Some([]) => rows.push(note(format!("{} has not run here yet, so its options are not known. It reviews as it starts.", reviewer.agent))),
            Some(options) => {
                for option in options {
                    let now = chosen(Some(reviewer), option);
                    let id = option.option_id.clone();
                    let picks: Vec<Pick> = match &option.value {
                        ControlValue::Boolean { .. } => {
                            [("On", "true"), ("Off", "false")].into_iter().map(|(l, v)| Pick { label: l.into(), value: Some(v.into()) }).collect()
                        }
                        ControlValue::Id { .. } => option.choices.iter().map(|c| Pick { label: c.title.clone(), value: Some(c.value.clone()) }).collect(),
                    };
                    let current = picks.iter().find(|p| p.value.as_deref() == Some(now.as_str())).cloned();
                    rows.push(setting(
                        option.title.as_str(),
                        None,
                        dropdown(picks, current, move |pick| Message::Crosscheck(Event::Choose(id.clone(), pick.value.unwrap_or_default()))),
                    ));
                }
                rows.push(note(format!("These are the options {} itself offers. They are the Reviewer's own: your sessions keep theirs.", reviewer.agent)));
            }
        }
        rows
    }
}

fn note<'a>(said: impl Into<String>) -> Element<'a, Message> {
    container(text(said.into()).size(style::TINY).color(P.fg_dim).width(Fill).wrapping(text::Wrapping::WordOrGlyph)).padding([10, 16]).into()
}

/// A working Reviewer's steps, the latest last and marked; always [`STEPS`] lines high.
fn steps<'a>(run: &'a api::Run) -> Element<'a, Message> {
    let all = run.reviewers.first().map(|p| p.activity.as_slice()).unwrap_or_default();
    let shown = &all[all.len().saturating_sub(STEPS)..];
    let mut list = Column::new().spacing(3);
    for (n, step) in shown.iter().enumerate() {
        let last = n + 1 == shown.len();
        let mark: Element<'a, Message> =
            if last { crate::ui::dot(P.accent) } else { container(icons::glyph("check", 10.0, alpha(P.fg, 0.4))).center_x(15).into() };
        list = list.push(
            row![mark, text(crate::ui::one_line(step, 160)).size(style::TINY).color(if last { P.fg } else { P.fg_dim }).wrapping(text::Wrapping::None)]
                .spacing(6)
                .height(STEP_HEIGHT)
                .align_y(Alignment::Center),
        );
    }
    if shown.is_empty() {
        list = list.push(row![text("Nothing yet.").size(style::TINY).color(P.fg_dim)].height(STEP_HEIGHT).align_y(Alignment::Center));
    }
    container(container(list).height(STEPS as f32 * (STEP_HEIGHT + 3.0) + 9.0).width(Fill).padding([6, 8]).clip(true).style(style::fill(alpha(P.fg, 0.04))))
        .padding(Padding { top: 2.0, right: 0.0, bottom: 4.0, left: 0.0 })
        .into()
}

const STEP_HEIGHT: f32 = 17.0;

/// A review that did not finish, opened: all of why.
fn failure<'a>(run: &'a api::Run) -> Element<'a, Message> {
    let why = run.reviewers.first().and_then(|p| p.error.as_deref()).filter(|e| !e.trim().is_empty()).unwrap_or(run.detail.as_str());
    let why = if why.trim().is_empty() { "The review did not finish." } else { why };
    container(text(why).size(style::SMALL).color(P.fg_dim).width(Fill).wrapping(text::Wrapping::WordOrGlyph))
        .padding(Padding { top: 2.0, right: 10.0, bottom: 6.0, left: 24.0 })
        .into()
}

#[cfg(test)]
#[path = "crosscheck_tests.rs"]
pub(crate) mod tests;
