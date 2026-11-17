//! The box's connection in the window (docs/sill-gui.md, Box restart recovery): one band for the
//! whole box when its shared SSH connection is gone — offline with the next retry and Retry now,
//! or Sign in to reconnect — and SSH's own questions during a sign-in. What is typed goes to
//! OpenSSH through `sill_proto::boxlink` and is kept nowhere.
use std::collections::VecDeque;

use iced::widget::{column, container, row, space, text, text_input};
use iced::{Alignment, Element, Fill, Task};
use sill_proto::boxlink::{self, Problem, Prompt, PromptKind};

use crate::style::{self, P, alpha};
use crate::{App, Message, blocking, keys};

/// One question from SSH and where its answer goes; dropping it answers "cancelled".
#[derive(Debug, Clone)]
pub struct Ask {
    pub prompt: Prompt,
    pub reply: std::sync::mpsc::Sender<Option<String>>,
}

#[derive(Debug, Clone)]
pub enum Event {
    SignIn(String),
    SignedIn(Result<(), String>),
    RetryNow(String),
    Asked(Ask),
    Input(String),
    Answer,
    Yes,
    Cancel,
}

#[derive(Default)]
pub struct State {
    /// SSH's questions, the first shown.
    asks: VecDeque<Ask>,
    input: String,
    signing_in: bool,
    /// Where the sign-in's prompter sends questions (`App::with` listens on the other end).
    pub asker: Option<async_channel::Sender<Ask>>,
    /// A box state to draw, for the snapshots.
    #[cfg(test)]
    pub fake_status: Option<boxlink::Status>,
}

impl State {
    /// What asks SSH's questions in this window's band, for a sign-in to any box (this one's, or
    /// one being added in Settings); None in the demo.
    pub(crate) fn prompter(&self) -> Option<boxlink::Prompter> {
        let asker = self.asker.clone()?;
        Some(std::sync::Arc::new(move |prompt| {
            let (reply, answer) = std::sync::mpsc::channel();
            asker.send_blocking(Ask { prompt, reply }).ok()?;
            answer.recv().ok().flatten()
        }))
    }

    fn answer(&mut self, answer: Option<String>) {
        if let Some(ask) = self.asks.pop_front() {
            let _ = ask.reply.send(answer);
        }
        self.input.clear();
    }
}

impl App {
    pub(crate) fn box_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::SignIn(host) => {
                let Some(prompter) = self.box_link.prompter() else {
                    return Task::none();
                };
                if self.box_link.signing_in {
                    return Task::none();
                }
                self.box_link.signing_in = true;
                blocking(
                    move || boxlink::sign_in(&host, Some(prompter)).map(|_| ()).map_err(|e| format!("{e:#}")),
                    |result| Message::Box(Event::SignedIn(result)),
                )
            }
            Event::SignedIn(result) => {
                self.box_link.signing_in = false;
                // questions SSH no longer waits for
                self.box_link.asks.clear();
                self.box_link.input.clear();
                if result.is_ok() {
                    return Task::batch([self.daemon_event(crate::daemon_ui::Event::Check), Task::done(Message::Refresh)]);
                }
                Task::none()
            }
            Event::RetryNow(host) => {
                boxlink::retry_now(&host);
                Task::batch([self.daemon_event(crate::daemon_ui::Event::Check), Task::done(Message::Refresh)])
            }
            Event::Asked(ask) => {
                let first = self.box_link.asks.is_empty();
                self.box_link.asks.push_back(ask);
                if first {
                    return iced::widget::operation::focus("box-answer");
                }
                Task::none()
            }
            Event::Input(input) => {
                self.box_link.input = input;
                Task::none()
            }
            Event::Answer => {
                let answer = std::mem::take(&mut self.box_link.input);
                self.box_link.answer(Some(answer));
                iced::widget::operation::focus("box-answer")
            }
            Event::Yes => {
                self.box_link.answer(Some(String::new()));
                Task::none()
            }
            Event::Cancel => {
                self.box_link.answer(None);
                Task::none()
            }
        }
    }
}

fn act<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(iced::widget::button(text(label).size(style::SMALL)).padding([2, 8]).style(style::tinted(P.accent)), message)
}

/// The window-level band: SSH's question when a sign-in is asking one, else the box's trouble.
pub fn band(app: &App) -> Option<Element<'_, Message>> {
    let state = &app.box_link;
    let body: Element<'_, Message> = if let Some(ask) = state.asks.front() {
        let prompt = &ask.prompt;
        let question = column![
            text(format!("SSH asks, to sign in to {}:", prompt.host)).size(style::TINY).color(P.fg_dim),
            text(prompt.text.trim().to_owned()).size(style::SMALL).color(P.fg),
        ]
        .spacing(2);
        let answer: Element<'_, Message> = match prompt.kind {
            PromptKind::Notice => space().height(0).into(),
            PromptKind::Confirm => row![act("Yes", Some(Message::Box(Event::Yes))), act("No", Some(Message::Box(Event::Cancel))),].spacing(8).into(),
            PromptKind::Secret | PromptKind::Text => row![
                text_input("", &state.input)
                    .id("box-answer")
                    .secure(prompt.kind == PromptKind::Secret)
                    .size(style::SMALL)
                    .padding(6)
                    .on_input(|input| Message::Box(Event::Input(input)))
                    .on_submit(Message::Box(Event::Answer)),
                act("OK", Some(Message::Box(Event::Answer))),
                act("Cancel", Some(Message::Box(Event::Cancel))),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        };
        column![question, answer].spacing(6).into()
    } else {
        #[cfg(test)]
        let status = state.fake_status.clone().or_else(boxlink::status)?;
        #[cfg(not(test))]
        let status = boxlink::status()?;
        let waits_for_person = matches!(status.problem, Problem::SignIn | Problem::HostKey);
        let headline = if state.signing_in || status.signing_in {
            format!("Signing in to {}…", status.host)
        } else if waits_for_person {
            format!("Sign in to reconnect to {}", status.host)
        } else {
            match status.retry_in {
                Some(wait) if !wait.is_zero() => format!("Box {} offline — reconnecting in {} s", status.host, wait.as_secs().max(1)),
                _ => format!("Box {} offline — reconnecting…", status.host),
            }
        };
        let action = if waits_for_person {
            act("Sign in", (!state.signing_in && !status.signing_in && !app.demo).then(|| Message::Box(Event::SignIn(status.host.clone()))))
        } else {
            act("Retry now", Some(Message::Box(Event::RetryNow(status.host.clone()))))
        };
        row![column![text(headline).size(style::SMALL).color(P.error), text(status.detail).size(style::TINY).color(P.fg_dim),].spacing(2).width(Fill), action,]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
    };
    Some(container(body).width(Fill).padding([6, 10]).style(style::fill(alpha(P.error, 0.12))).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(app: &mut App, text: &str, kind: PromptKind) -> std::sync::mpsc::Receiver<Option<String>> {
        let (reply, answer) = std::sync::mpsc::channel();
        let prompt = Prompt { host: "box.test".into(), text: text.into(), kind };
        let _ = app.box_event(Event::Asked(Ask { prompt, reply }));
        answer
    }

    #[test]
    fn ssh_questions_are_answered_in_order_and_what_was_typed_is_not_kept() {
        let (mut app, _) = App::new(true, None);
        assert!(band(&app).is_none(), "no band while the box is fine");
        let first = ask(&mut app, "Enter passphrase for key 'k':", PromptKind::Secret);
        let second = ask(&mut app, "user@box's password:", PromptKind::Secret);
        assert!(band(&app).is_some());
        let _ = app.box_event(Event::Input("open sesame".into()));
        let _ = app.box_event(Event::Answer);
        assert_eq!(first.try_recv().unwrap().as_deref(), Some("open sesame"));
        assert!(app.box_link.input.is_empty(), "the answer leaves the window with its question");
        assert!(second.try_recv().is_err(), "the second question waits its turn");
        let _ = app.box_event(Event::Cancel);
        assert_eq!(second.try_recv().unwrap(), None);
        assert!(band(&app).is_none());
    }

    #[test]
    fn a_finished_sign_in_withdraws_its_open_questions() {
        let (mut app, _) = App::new(true, None);
        let open = ask(&mut app, "Enter passphrase for key 'k':", PromptKind::Secret);
        let _ = app.box_event(Event::Input("half typed".into()));
        let _ = app.box_event(Event::SignedIn(Err("box box.test is unreachable".into())));
        assert!(open.recv().is_err(), "the waiting helper is released as cancelled");
        assert!(app.box_link.input.is_empty() && band(&app).is_none());
    }
}
