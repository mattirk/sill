mod backend;
mod demo;
mod model;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::mpsc};

use clap::Parser;
use iced::widget::{
    button, column, container, markdown, pick_list, row, scrollable, text, text_editor,
};
use iced::{Element, Fill, Font, Subscription, Task, Theme};
use sill_proto::sfp::SessionEvent;

use backend::{Command, Update};
use model::{Conversation, item_text};

#[derive(Parser)]
#[command(about = "Experimental read-only Iced conversation viewer for Sill")]
struct Options {
    /// Attach to an existing session by exact ID, unique prefix or title.
    #[arg(long, conflicts_with = "demo")]
    session: Option<String>,
    /// Show a synthetic streaming conversation without connecting to a daemon.
    #[arg(long)]
    demo: bool,
}

fn main() -> iced::Result {
    let options = Options::parse();
    iced::application(
        move || App::new(options.demo, options.session.clone()),
        App::update,
        App::view,
    )
    .title("Sill · Iced preview")
    .theme(Theme::Dracula)
    .subscription(App::subscription)
    .font(include_bytes!("../../sill/fonts/SourceSans3.ttf").as_slice())
    .font(include_bytes!("../../sill/fonts/SourceSans3-Italic.ttf").as_slice())
    .font(include_bytes!("../../sill/fonts/SourceCodePro.ttf").as_slice())
    .default_font(Font::with_name("Source Sans 3"))
    .window_size((760.0, 900.0))
    .run()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionChoice {
    id: String,
    title: String,
}

impl std::fmt::Display for SessionChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} · {}",
            self.title,
            self.id.chars().take(8).collect::<String>()
        )
    }
}

#[derive(Debug, Clone)]
enum Message {
    Backend(u64, Update),
    Choose(SessionChoice),
    Refresh,
    Reconnect,
    Copy(String),
    Link(String),
    ToggleText,
    RefreshText,
    TextAction(text_editor::Action),
    DemoTick,
    Scrolled(bool),
    Latest,
}

struct Rendered {
    source: String,
    markdown: markdown::Content,
}

struct App {
    commands: Option<mpsc::Sender<Command>>,
    choices: Vec<SessionChoice>,
    selected: Option<SessionChoice>,
    conversation: Conversation,
    rendered: BTreeMap<String, Rendered>,
    status: String,
    error: Option<String>,
    text_view: Option<text_editor::Content>,
    demo: bool,
    demo_step: usize,
    loading: bool,
    generation: u64,
    follow_tail: bool,
}

impl App {
    fn new(demo: bool, initial: Option<String>) -> (Self, Task<Message>) {
        let mut app = Self {
            commands: None,
            choices: Vec::new(),
            selected: None,
            conversation: Conversation::default(),
            rendered: BTreeMap::new(),
            status: "Connecting…".into(),
            error: None,
            text_view: None,
            demo,
            demo_step: 0,
            loading: false,
            generation: 0,
            follow_tail: true,
        };
        if demo {
            app.conversation = demo::conversation();
            app.status = "Demo · streaming".into();
            app.sync_rendered();
            return (app, Task::none());
        }
        let (commands, events) = backend::start(initial);
        app.commands = Some(commands);
        let stream = iced::futures::stream::unfold(events, |events| async move {
            events.recv().await.ok().map(|event| (event, events))
        });
        (
            app,
            Task::run(stream, |(generation, update)| {
                Message::Backend(generation, update)
            }),
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.demo && self.demo_step < demo::CHUNKS.len() {
            iced::time::every(std::time::Duration::from_millis(600)).map(|_| Message::DemoTick)
        } else {
            Subscription::none()
        }
    }

    fn command(&mut self, command: Command) {
        if self
            .commands
            .as_ref()
            .is_some_and(|tx| tx.send(command).is_err())
        {
            self.error = Some("Connection worker stopped. Reopen the preview.".into());
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if let Message::Backend(generation, _) = &message
            && *generation != self.generation
        {
            return Task::none();
        }
        let scroll_to_tail = self.follow_tail
            && matches!(
                &message,
                Message::Backend(_, Update::Snapshot(_) | Update::Event(_)) | Message::DemoTick
            );
        match message {
            Message::Backend(_, Update::Sessions(sessions)) => {
                self.choices = sessions
                    .into_iter()
                    .filter(|s| s.ended_at.is_none())
                    .map(|s| SessionChoice {
                        id: s.session_id,
                        title: s.title,
                    })
                    .collect();
            }
            Message::Backend(_, Update::Snapshot(snapshot)) => {
                self.selected = self
                    .choices
                    .iter()
                    .find(|s| s.id == snapshot.session_id)
                    .cloned();
                self.conversation = *snapshot;
                self.rendered.clear();
                self.text_view = None;
                self.loading = false;
                self.error = None;
                self.sync_rendered();
            }
            Message::Backend(_, Update::Event(event)) => self.accept(*event),
            Message::Backend(_, Update::Status(status)) => {
                if status == "Connected" {
                    self.loading = false;
                    self.error = None;
                }
                self.status = status;
            }
            Message::Backend(_, Update::Error(error)) => {
                self.status = "Disconnected · retrying".into();
                self.error = Some(error);
                self.loading = false;
            }
            Message::Choose(choice) => {
                self.generation += 1;
                self.command(Command::Attach {
                    id: choice.id.clone(),
                    generation: self.generation,
                });
                self.selected = Some(choice);
                self.loading = true;
                self.follow_tail = true;
                self.conversation = Conversation::default();
                self.rendered.clear();
                self.text_view = None;
                self.status = "Loading conversation…".into();
                self.error = None;
            }
            Message::Refresh => self.command(Command::Refresh),
            Message::Scrolled(at_bottom) => self.follow_tail = at_bottom,
            Message::Latest => {
                self.follow_tail = true;
                return iced::widget::operation::snap_to_end("conversation");
            }
            Message::Reconnect => self.reconnect(),
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::Link(url) => {
                self.status = "Link copied".into();
                return iced::clipboard::write(url);
            }
            Message::ToggleText => {
                self.text_view = if self.text_view.is_some() {
                    None
                } else {
                    Some(text_editor::Content::with_text(
                        &self.conversation.transcript(),
                    ))
                };
            }
            Message::RefreshText => {
                self.text_view = Some(text_editor::Content::with_text(
                    &self.conversation.transcript(),
                ));
            }
            Message::TextAction(action) => {
                if !matches!(action, text_editor::Action::Edit(_))
                    && let Some(content) = &mut self.text_view
                {
                    content.perform(action);
                }
            }
            Message::DemoTick => {
                if let Some(chunk) = demo::CHUNKS.get(self.demo_step) {
                    self.accept(demo::append(self.conversation.seq + 1, chunk));
                    self.demo_step += 1;
                    if self.demo_step == demo::CHUNKS.len() {
                        self.status = "Demo · finished".into();
                    }
                }
            }
        }
        if scroll_to_tail && self.text_view.is_none() {
            iced::widget::operation::snap_to_end("conversation")
        } else {
            Task::none()
        }
    }

    fn reconnect(&mut self) {
        self.generation += 1;
        self.command(Command::Reconnect(self.generation));
        self.status = "Restoring conversation…".into();
    }

    fn accept(&mut self, event: SessionEvent) {
        match self.conversation.accept(event) {
            Ok(true) => self.sync_rendered(),
            Ok(false) => {}
            Err(error) => {
                self.error = Some(format!("{error:#}. Restoring the conversation."));
                self.reconnect();
            }
        }
    }

    fn sync_rendered(&mut self) {
        self.rendered
            .retain(|id, _| self.conversation.items.contains_key(id));
        for item in self.conversation.items.values() {
            let source = item_text(item);
            match self.rendered.get_mut(&item.item_id) {
                Some(cached) if cached.source == source => {}
                Some(cached) if source.starts_with(&cached.source) => {
                    cached.markdown.push_str(&source[cached.source.len()..]);
                    cached.source = source;
                }
                _ => {
                    self.rendered.insert(
                        item.item_id.clone(),
                        Rendered {
                            markdown: markdown::Content::parse(&source),
                            source,
                        },
                    );
                }
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let mut header = row![
            text("Sill").size(24),
            text("Iced preview · read-only").size(14)
        ]
        .spacing(16)
        .align_y(iced::Center);
        if !self.demo {
            header = header
                .push(button("Refresh").on_press(Message::Refresh))
                .push(button("Reconnect").on_press(Message::Reconnect));
        }
        let mut layout = column![header].spacing(12);
        if !self.demo {
            if self.loading {
                layout = layout.push(text("Loading conversation…"));
            } else {
                layout = layout.push(
                    pick_list(&self.choices[..], self.selected.as_ref(), Message::Choose)
                        .placeholder("Choose a session")
                        .width(Fill),
                );
            }
        }
        if let Some(error) = &self.error {
            layout = layout.push(
                container(iced_selection::text(error).size(14))
                    .padding(10)
                    .style(container::rounded_box),
            );
        }
        let activity = self
            .conversation
            .session
            .as_ref()
            .map_or("", |s| s.activity.as_str());
        layout =
            layout.push(row![text(&self.status).size(13), text(activity).size(13)].spacing(12));
        if !self.conversation.session_id.is_empty() {
            layout = layout.push(
                row![
                    button(if self.text_view.is_some() {
                        "Formatted view"
                    } else {
                        "Select transcript text"
                    })
                    .on_press(Message::ToggleText),
                    button("Copy transcript")
                        .on_press_with(|| Message::Copy(self.conversation.transcript())),
                    button("Latest").on_press(Message::Latest),
                ]
                .spacing(8),
            );
        }
        if let Some(content) = &self.text_view {
            layout = layout
                .push(
                    row![
                        text("Text captured when opened"),
                        button("Refresh text").on_press(Message::RefreshText)
                    ]
                    .spacing(12),
                )
                .push(
                    text_editor(content)
                        .on_action(Message::TextAction)
                        .height(Fill)
                        .font(Font::MONOSPACE),
                );
        } else {
            let settings = markdown::Settings::with_text_size(
                16,
                markdown::Style::from_palette(Theme::Dracula.palette()),
            );
            let mut feed = column![].spacing(18);
            for item in self.conversation.visible_items() {
                if let Some(cached) = self.rendered.get(&item.item_id) {
                    let heading = row![
                        text(&item.kind).size(13),
                        button("Copy").on_press_with(|| Message::Copy(cached.source.clone()))
                    ]
                    .spacing(12)
                    .align_y(iced::Center);
                    let body = iced_selection::markdown(cached.markdown.items(), settings)
                        .map(Message::Link);
                    feed = feed.push(
                        container(column![heading, body].spacing(8))
                            .padding(14)
                            .width(Fill)
                            .style(container::rounded_box),
                    );
                }
            }
            for question in self
                .conversation
                .questions
                .values()
                .filter(|q| q.state == "open")
            {
                let mut card = column![text(&question.title).size(20)].spacing(8);
                if let Some(description) = &question.description {
                    card = card.push(iced_selection::text(description));
                }
                if let Some(subject) = &question.subject
                    && let Some(text) = &subject.text
                {
                    card = card.push(iced_selection::text(text));
                }
                for option in &question.options {
                    card = card.push(text(&option.title));
                }
                for field in &question.fields {
                    card = card.push(text(&field.title));
                }
                card = card.push(text("Answer in the existing Sill shelf.").size(13));
                feed = feed.push(
                    container(card)
                        .padding(16)
                        .width(Fill)
                        .style(container::rounded_box),
                );
            }
            if self.conversation.session_id.is_empty() {
                feed = feed.push(text(
                    "Choose a session to view its conversation alongside the existing shelf.",
                ));
            }
            layout = layout.push(
                scrollable(feed)
                    .id("conversation")
                    .height(Fill)
                    .on_scroll(|viewport| Message::Scrolled(viewport.relative_offset().y >= 0.99)),
            );
        }
        container(layout)
            .padding(20)
            .width(Fill)
            .height(Fill)
            .into()
    }
}
