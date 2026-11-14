//! The window as the shelf draws it (`docs/reference-ui` 01–02 and 19–22): a top bar with the
//! sill mark, one tab per open session and the selected agent's activity at the right; the
//! conversation; the composer with the agent's controls and send / stop.
//!
//! Supervision (docs/sill-gui.md, direction 4): every tab carries its session's mark — working,
//! a question waiting, a problem — and while a file or the transcript covers the conversation a
//! status strip keeps the selected session's state, its question and Stop in reach.
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Instant;

use iced::keyboard::{Key, key::Named};
use iced::widget::markdown::{self, Highlight};
use iced::widget::{
    Column, button, canvas, center, column, container, mouse_area, pick_list, responsive, right,
    row, rule, scrollable, space, stack, svg, text, text_editor, tooltip,
};
use iced::{
    Alignment, Color, Element, Fill, Font, Padding, Radians, Rectangle, Renderer, Shrink, Theme,
};
use sill_client::lane::Status;
use sill_client::outbox::State;
use sill_client::supervision::{Activity, Attention, Level, clock};
use sill_proto::sfp::{Answer, Item, Question, Tool};

use crate::style::{self, P, alpha};
use crate::{App, Message, Pick, SessionView, Tab, icons, keys, selection, viewer};

const MONO_FONT: Font = Font::with_name(crate::fonts::MONO_FAMILY);
/// Below this width the drawer takes the content area instead of sitting beside it.
pub const NARROW: f32 = 600.0;

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let content = responsive(move |size| {
            let Some(view) = self.current() else {
                return self.empty();
            };
            let covered = !view.conversation_shown();
            let body = match self.file_view(view) {
                Some(reader) => reader,
                None => {
                    let conversation = match &view.text_view {
                        Some(content) => self.transcript_view(content),
                        None => self.feed(view),
                    };
                    column![conversation, self.composer(view)]
                        .height(Fill)
                        .into()
                }
            };
            let main = column![self.file_chips(view), body]
                .width(Fill)
                .height(Fill);
            let narrow = size.width < NARROW;
            // the conversation keeps one place in the widget tree whether the drawer is beside it
            // or not: moving it would give its scrollable fresh state (back at the top) and drop
            // a selection in it
            let page: Element<'_, Message> = if self.workspace.shown && narrow {
                self.file_drawer(view).map(|message| match message {
                    Message::Workspace(crate::workspace::Action::Open { path, repo }) => {
                        Message::Workspace(crate::workspace::Action::OpenAndHide { path, repo })
                    }
                    message => message,
                })
            } else {
                let mut page = row![main].height(Fill);
                if self.workspace.shown {
                    page = page
                        .push(rule::vertical(1).style(style::rule_style(P.border)))
                        .push(
                            container(self.file_drawer(view))
                                .width((size.width * 0.28).clamp(205.0, 320.0)),
                        );
                }
                page.into()
            };
            // the conversation is out of sight: its state, question and Stop stay in reach
            let mut screen = column![page].height(Fill);
            if covered || (self.workspace.shown && narrow) {
                screen = screen.push(self.strip(view));
            }
            screen.into()
        });
        column![self.top_bar(), content].into()
    }

    /// No session selected: why, and what to do.
    fn empty(&self) -> Element<'_, Message> {
        let hint = if let Some(error) = &self.list_error {
            format!("The daemon cannot be reached: {error}")
        } else if !self.listed {
            "Connecting…".into()
        } else if self.tabs.is_empty() {
            "No open session. Start one in the shelf; it shows here as a tab.".into()
        } else {
            "No session is running. Choose one above to open it.".into()
        };
        container(center(text(hint).size(style::TEXT).color(
            if self.list_error.is_some() {
                P.error
            } else {
                P.fg_dim
            },
        )))
        .padding(24)
        .height(Fill)
        .into()
    }

    // --- top bar --------------------------------------------------------------------------------

    fn top_bar(&self) -> Element<'_, Message> {
        let mark = svg(icons::herring())
            .width(18)
            .height(18)
            .style(style::icon(P.fg));
        let mut tabs = row![].spacing(2).height(Fill);
        for tab in &self.tabs {
            tabs = tabs.push(self.tab(tab));
        }
        let select = tooltip(
            keys::press_maybe(
                button(text("⌶").size(style::TEXT))
                    .padding([2, 8])
                    .style(style::quiet),
                self.current().map(|_| Message::ToggleText),
            ),
            container(text("The conversation as one text").size(style::TINY))
                .padding([3, 8])
                .style(style::fill(P.surface)),
            tooltip::Position::Bottom,
        );
        // the window has no title bar: a press here that no tab or button takes moves it
        mouse_area(
            container(
                row![
                    container(mark).padding(Padding::from([0.0, 14.0])),
                    scrollable(tabs)
                        .width(Fill)
                        .height(Fill)
                        .direction(scrollable::Direction::Horizontal(
                            scrollable::Scrollbar::default()
                        ))
                        .style(style::feed_scroll),
                    self.activity(),
                    select,
                    keys::press(
                        button(
                            text(if self.workspace.shown {
                                "›"
                            } else {
                                "Files ‹"
                            })
                            .size(style::SMALL)
                        )
                        .padding([2, 8])
                        .style(style::quiet),
                        Message::Workspace(crate::workspace::Action::ToggleDrawer)
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .height(37)
            .padding(Padding {
                top: 0.0,
                right: 8.0,
                bottom: 0.0,
                left: 0.0,
            })
            .width(Fill)
            .style(style::fill(P.bg)),
        )
        .on_press(Message::DragWindow)
        .into()
    }

    /// A tab: the agent's glyph, the title, and the session's mark — accent while working,
    /// warning while a question waits, error for a problem. Dimmer when the session is not running.
    fn tab<'a>(&'a self, tab: &'a Tab) -> Element<'a, Message> {
        let active = self.selected.as_deref() == Some(tab.id.as_str());
        let view = self.views.get(&tab.id);
        let live = view.is_some_and(|v| !v.status.ended()) || tab.running;
        let color = match (active, live) {
            (true, _) => P.fg,
            (false, true) => alpha(P.fg, 0.6),
            (false, false) => alpha(P.fg, 0.35),
        };
        let mut label = row![
            svg(icons::for_agent(&tab.agent))
                .width(13)
                .height(13)
                .style(style::icon(color)),
            text(&tab.title).size(style::TEXT).color(color),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let attention = view.map(SessionView::attention);
        if let Some(mark) = attention.as_ref().and_then(mark) {
            label = label.push(mark);
        }
        let label = keys::press(
            button(label).padding([4, 12]).style(style::quiet),
            Message::Choose(tab.id.clone()),
        );
        let underline = rule::horizontal(2).style(style::rule_style(if active {
            P.accent
        } else {
            Color::TRANSPARENT
        }));
        let tab_body = column![
            container(label).height(Fill).align_y(Alignment::Center),
            underline
        ]
        .width(Shrink)
        .height(Fill);
        match attention.filter(|a| a.level() >= Level::Problem) {
            Some(attention) => tooltip(
                tab_body,
                container(text(attention.summary()).size(style::TINY))
                    .padding([3, 8])
                    .style(style::fill(P.surface)),
                tooltip::Position::Bottom,
            )
            .into(),
            None => tab_body.into(),
        }
    }

    /// Right of the tabs: the timer and a spinner while the agent works, the spinner alone while
    /// a question waits, otherwise a dot — grey when all is well, red when disconnected.
    fn activity(&self) -> Element<'_, Message> {
        let Some(view) = self.current() else {
            return status_dot(
                if self.list_error.is_some() {
                    P.error
                } else {
                    alpha(P.fg, 0.35)
                },
                self.list_error.clone().unwrap_or_default(),
            );
        };
        let attention = view.attention();
        let trouble = view
            .status
            .problem()
            .map(str::to_owned)
            .or_else(|| self.list_error.clone());
        if trouble.is_none() && matches!(attention.activity, Activity::Working | Activity::Waiting)
        {
            let mut parts = row![].spacing(8).align_y(Alignment::Center);
            if attention.activity == Activity::Working
                && let Some(ms) = attention.working_ms
            {
                parts = parts.push(
                    text(clock(ms))
                        .size(style::TEXT)
                        .font(MONO_FONT)
                        .color(P.fg_dim),
                );
            }
            return parts.push(self.spinner(P.accent)).into();
        }
        match trouble {
            Some(error) => status_dot(P.error, error),
            None => status_dot(alpha(P.fg, 0.35), status_text(&view.status).into()),
        }
    }

    fn spinner(&self, color: Color) -> Element<'_, Message> {
        canvas(Spinner {
            angle: spin(self.now),
            color,
        })
        .width(16)
        .height(16)
        .into()
    }

    // --- supervision ----------------------------------------------------------------------------

    /// While the conversation is out of sight: the selected session's state, its first question
    /// with its options, other sessions that need the person, the way back, and Stop.
    fn strip<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let attention = view.attention();
        let color = match attention.level() {
            Level::Question => P.warning,
            Level::Problem => P.error,
            Level::Working => P.accent,
            Level::Calm => P.fg_dim,
        };
        let mut bar = row![].spacing(8).align_y(Alignment::Center);
        if attention.level() == Level::Working {
            bar = bar.push(self.spinner(P.accent));
        }
        let mut summary = attention.summary();
        if attention.activity == Activity::Working
            && attention.questions.is_empty()
            && let Some(ms) = attention.working_ms
        {
            summary = format!("{summary} · {}", clock(ms));
        }
        bar = bar.push(
            container(
                text(one_line(&summary, 90))
                    .size(style::SMALL)
                    .color(color)
                    .wrapping(text::Wrapping::None),
            )
            .width(Fill)
            .clip(true),
        );
        // a permission question can be answered from here; a form needs the conversation
        if let Some(question) = view.conversation.open_questions().next()
            && question.fields.is_empty()
        {
            for option in &question.options {
                bar = bar.push(keys::press_maybe(
                    button(text(&option.title).size(style::TINY))
                        .padding([2, 8])
                        .style(option_style(&option.kind)),
                    (!view.answering.contains(&question.question_id)).then(|| Message::Answer {
                        question_id: question.question_id.clone(),
                        option_id: option.option_id.clone(),
                    }),
                ));
            }
        }
        for tab in &self.tabs {
            if self.selected.as_deref() == Some(tab.id.as_str()) {
                continue;
            }
            let Some(other) = self.views.get(&tab.id).map(SessionView::attention) else {
                continue;
            };
            if other.level() < Level::Problem {
                continue;
            }
            let label = format!(
                "{} {}",
                if other.level() == Level::Question {
                    "?"
                } else {
                    "!"
                },
                one_line(&tab.title, 18)
            );
            bar = bar.push(tooltip(
                keys::press(
                    button(text(label).size(style::TINY).color(
                        if other.level() == Level::Question {
                            P.warning
                        } else {
                            P.error
                        },
                    ))
                    .padding([2, 6])
                    .style(style::quiet),
                    Message::Choose(tab.id.clone()),
                ),
                container(text(other.summary()).size(style::TINY))
                    .padding([3, 8])
                    .style(style::fill(P.surface)),
                tooltip::Position::Top,
            ));
        }
        // a file's reader has its own way back
        if view.files.reader.is_none() {
            bar = bar.push(keys::press(
                button(text("‹ Conversation").size(style::TINY))
                    .padding([2, 8])
                    .style(style::quiet),
                Message::ShowConversation,
            ));
        }
        if view.conversation.busy() {
            bar = bar.push(stop_button());
        }
        column![
            rule::horizontal(1).style(style::rule_style(P.border)),
            container(bar)
                .padding([4, 10])
                .width(Fill)
                .style(style::fill(P.surface)),
        ]
        .into()
    }

    // --- the conversation -----------------------------------------------------------------------

    fn feed<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let conversation = &view.conversation;
        if conversation.session_id.is_empty() {
            let (hint, color) = match &view.status {
                Status::Reconnecting(error) => (format!("Connecting… {error}"), P.error),
                Status::NotRunning => (
                    "Not running. Send a prompt, or choose the tab again, to start it.".into(),
                    P.fg_dim,
                ),
                Status::Gone(why) | Status::Stopped(why) => (why.clone(), P.error),
                _ => ("Loading conversation…".into(), P.fg_dim),
            };
            return container(center(text(hint).size(style::TEXT).color(color)))
                .padding(24)
                .height(Fill)
                .into();
        }
        let mut feed = Column::new().spacing(12).padding(Padding {
            top: 10.0,
            right: 14.0,
            bottom: 18.0,
            left: 12.0,
        });
        let questions: Vec<&Question> = conversation.questions.values().collect();
        let mut placed = BTreeSet::new();
        let items = conversation.visible_items();
        for (index, item) in items.iter().enumerate() {
            feed = feed.push(self.item(view, item));
            for question in questions
                .iter()
                .filter(|q| q.tool_item_id.as_deref() == Some(item.item_id.as_str()))
            {
                placed.insert(question.question_id.as_str());
                feed = feed.push(self.question(view, question));
            }
            // the last item of a turn that was stopped or failed: say so under it
            if let Some(turn_id) = &item.turn_id
                && items.get(index + 1).and_then(|next| next.turn_id.as_ref()) != Some(turn_id)
                && let Some(end) = conversation.turn_end(turn_id)
            {
                feed = feed.push(
                    container(
                        selection::text(format!("· {end}"))
                            .size(style::SMALL)
                            .style(style::selectable(P.fg_dim)),
                    )
                    .padding([0, 8]),
                );
            }
        }
        for question in questions
            .iter()
            .filter(|q| !placed.contains(q.question_id.as_str()))
        {
            feed = feed.push(self.question(view, question));
        }
        if let Some(error) = view.status.problem() {
            feed = feed.push(note(error, P.error));
        }
        if let Some(failed) = &view.failed {
            feed = feed.push(note(failed, P.error));
        }
        let scroll = scrollable(feed)
            .id("conversation")
            .height(Fill)
            .width(Fill)
            .style(style::feed_scroll)
            .on_scroll(|viewport| {
                Message::Scrolled(
                    viewport.relative_offset().y >= 0.99,
                    viewport.absolute_offset().y,
                )
            });
        // the scrollable stays the stack's first layer whether or not "↓ Latest" is over it:
        // were it the root one moment and a layer the next, its state (where it is scrolled to)
        // would be new each time tail-follow turned off
        let mut layers = stack![scroll];
        if !view.follow_tail {
            layers = layers.push(
                right(keys::press(
                    button(text("↓ Latest").size(style::SMALL))
                        .padding([4, 10])
                        .style(style::solid(P.surface)),
                    Message::Latest,
                ))
                .padding(Padding {
                    top: 0.0,
                    right: 18.0,
                    bottom: 12.0,
                    left: 0.0,
                })
                .align_y(Alignment::End)
                .height(Fill),
            );
        }
        // one selection across every message
        selection::area(layers)
            .on_copy(Message::Copy)
            .on_drag(Message::Selecting)
            .into()
    }

    fn item<'a>(&'a self, view: &'a SessionView, item: &'a Item) -> Element<'a, Message> {
        if let Some(tool) = &item.tool {
            return self.tool(view, item, tool);
        }
        let source = view
            .rendered
            .get(&item.item_id)
            .map_or("", |r| r.source.as_str());
        match item.kind.as_str() {
            // the person's own words: a framed box on the code background behind the accent rule
            "user" => ruled(
                3.0,
                alpha(P.accent, 0.9),
                container(
                    selection::text(source)
                        .size(style::TEXT)
                        .style(style::selectable_inherited),
                )
                .padding([10, 14])
                .width(Fill)
                .style(style::fill(P.code_bg)),
            ),
            "thought" => container(
                selection::text(source)
                    .size(style::SMALL)
                    .style(style::selectable(alpha(P.fg, 0.55))),
            )
            .padding([0, 8])
            .into(),
            "note" => container(
                selection::text(format!("· {source}"))
                    .size(style::SMALL)
                    .style(style::selectable(P.fg_dim)),
            )
            .padding([0, 8])
            .into(),
            "notice" => {
                let severity = item
                    .content
                    .first()
                    .and_then(|b| b.extra.get("severity"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("info");
                let color = match severity {
                    "error" => P.error,
                    "warning" => P.warning,
                    _ => P.fg_dim,
                };
                note(source, color)
            }
            "plan" => plan(item),
            // agent text and anything newer: Markdown as the agent wrote it
            _ => match view.rendered.get(&item.item_id) {
                Some(rendered) => container(markdown::view_with(
                    rendered.markdown.items(),
                    markdown_settings(),
                    &viewer::SillViewer,
                ))
                .padding([0, 8])
                .width(Fill)
                .into(),
                None => space().into(),
            },
        }
    }

    /// A tool call: one quiet line — kind glyph, title, status mark — that opens to its output and
    /// diff (`.sill-tool`, `.sill-tool-line`, `.sill-tool-body`).
    fn tool<'a>(
        &'a self,
        view: &'a SessionView,
        item: &'a Item,
        tool: &'a Tool,
    ) -> Element<'a, Message> {
        let (mark, mark_color) = status_mark(&tool.status);
        let line = button(
            row![
                text(kind_glyph(&tool.kind))
                    .font(MONO_FONT)
                    .size(style::SMALL)
                    .color(alpha(P.fg, 0.6))
                    .width(16),
                container(
                    text(one_line(&tool.title, 140))
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::None),
                )
                .width(Fill)
                .clip(true),
                text(mark).size(style::SMALL).color(mark_color),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([3, 6])
        .width(Fill)
        .style(style::row_button);
        let line = keys::press(line, Message::ToggleTool(item.item_id.clone()));
        let mut body = Column::new().push(line);
        if view.open_tools.contains(&item.item_id) {
            if !tool.detail.trim().is_empty() {
                body = body.push(
                    container(
                        selection::text(tool.detail.trim_end())
                            .font(MONO_FONT)
                            .size(style::MONO)
                            .style(style::selectable(alpha(P.fg, 0.8))),
                    )
                    .padding(Padding {
                        top: 4.0,
                        right: 10.0,
                        bottom: 6.0,
                        left: 28.0,
                    }),
                );
            }
            if !tool.diff.trim().is_empty() {
                body = body.push(diff(&tool.diff));
            }
            if tool.detail.trim().is_empty() && tool.diff.trim().is_empty() {
                body = body.push(
                    container(text("No output.").size(style::TINY).color(P.fg_dim)).padding(
                        Padding {
                            top: 2.0,
                            right: 10.0,
                            bottom: 4.0,
                            left: 28.0,
                        },
                    ),
                );
            }
        }
        ruled(2.0, alpha(P.fg, 0.18), body.width(Fill))
    }

    /// A question: open, the card with the agent's options as buttons; answered, one line with
    /// the answer that opens to what was asked (`.sill-permission`, `.sill-answered-toggle`).
    fn question<'a>(
        &'a self,
        view: &'a SessionView,
        question: &'a Question,
    ) -> Element<'a, Message> {
        let mut card = Column::new().spacing(8);
        if question.state == "open" {
            card = card.push(
                selection::text(&question.title)
                    .size(style::TEXT)
                    .style(style::selectable_inherited),
            );
            card = asked(card, question);
            if question.fields.is_empty() {
                let sending = view.answering.contains(&question.question_id);
                let mut options = row![].spacing(8);
                for option in &question.options {
                    options = options.push(keys::press_maybe(
                        button(text(&option.title).size(style::TEXT))
                            .padding([6, 12])
                            .style(option_style(&option.kind)),
                        (!sending).then(|| Message::Answer {
                            question_id: question.question_id.clone(),
                            option_id: option.option_id.clone(),
                        }),
                    ));
                }
                card = card.push(options.wrap().vertical_spacing(8));
            } else {
                for field in &question.fields {
                    card = card
                        .push(text(format!("{} · {}", field.title, field.kind)).size(style::SMALL));
                }
                card = card.push(
                    text("Forms are answered in the shelf for now.")
                        .size(style::TINY)
                        .color(P.fg_dim),
                );
            }
        } else {
            let open = view.open_answers.contains(&question.question_id);
            let outcome = match &question.answer {
                Some(Answer::Permission { option_id }) => question
                    .options
                    .iter()
                    .find(|o| &o.option_id == option_id)
                    .map_or(option_id.as_str(), |o| o.title.as_str())
                    .to_string(),
                Some(Answer::Cancel) => "cancelled".into(),
                Some(Answer::Form { .. }) => "answered".into(),
                None => question.state.clone(),
            };
            card = card.push(
                selection::text(format!("{} → {outcome}", question.title))
                    .size(style::TEXT)
                    .style(style::selectable_inherited),
            );
            card = card.push(keys::press(
                button(text(if open { "▾ hide" } else { "▸ show" }).size(style::TINY))
                    .padding([0, 4])
                    .style(style::quiet),
                Message::ToggleAnswered(question.question_id.clone()),
            ));
            if open {
                card = asked(card, question);
            }
        }
        ruled(
            2.0,
            P.warning,
            container(card)
                .padding([8, 10])
                .width(Fill)
                .style(style::fill(alpha(P.warning, 0.10))),
        )
    }

    fn transcript_view<'a>(&'a self, content: &'a text_editor::Content) -> Element<'a, Message> {
        column![
            row![
                text("The conversation as text, captured when opened")
                    .size(style::SMALL)
                    .color(P.fg_dim),
                space().width(Fill),
                keys::press(
                    button(text("Refresh").size(style::SMALL)).style(style::quiet),
                    Message::RefreshText
                ),
                keys::press(
                    button(text("Copy all").size(style::SMALL)).style(style::quiet),
                    Message::CopyTranscript
                ),
                keys::press(
                    button(text("Close").size(style::SMALL)).style(style::quiet),
                    Message::ToggleText
                ),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            text_editor(content)
                .on_action(Message::TextAction)
                .key_binding(reading_keys)
                .height(Fill)
                .font(MONO_FONT)
                .size(style::MONO)
                .style(style::reader),
        ]
        .spacing(8)
        .padding(12)
        .height(Fill)
        .into()
    }

    // --- the composer ---------------------------------------------------------------------------

    fn composer<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let has_session = !view.conversation.session_id.is_empty() || self.demo;
        let mut composer = Column::new();
        for submission in view
            .conversation
            .submissions
            .values()
            .filter(|s| s.delivery == "queued")
        {
            composer = composer.push(
                row![
                    text("Queued").size(style::TINY).color(P.fg_dim),
                    container(
                        text(one_line(&submission.text, 120))
                            .size(style::SMALL)
                            .wrapping(text::Wrapping::None),
                    )
                    .width(Fill)
                    .clip(true),
                    keys::press(
                        button(text("×").size(style::SMALL))
                            .padding([0, 6])
                            .style(style::quiet),
                        Message::Unqueue(submission.submission_id.clone())
                    ),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .padding([2, 12]),
            );
        }
        composer = outbox(composer, view);
        let editor = text_editor(&view.draft)
            .id("prompt")
            .placeholder(placeholder(view))
            .on_action(Message::Prompt)
            .key_binding(enter_sends)
            .padding([8, 14])
            .size(style::TEXT)
            .height(Shrink)
            .max_height(220)
            .style(style::prompt);
        let action: Element<'_, Message> = if view.conversation.busy() {
            stop_button()
        } else {
            keys::press_maybe(
                button(
                    svg(icons::send())
                        .width(14)
                        .height(14)
                        .style(style::icon(P.bg)),
                )
                .padding(6)
                .style(style::solid(P.accent)),
                (!view.draft.text().trim().is_empty()).then_some(Message::Send),
            )
        };
        let shell = button(text(">_").size(style::SMALL).font(MONO_FONT).color(
            if view.terminal_shown {
                P.accent
            } else {
                P.fg_dim
            },
        ))
        .padding([2, 6])
        .style(style::quiet);
        let shell = keys::press_maybe(shell, has_session.then_some(Message::ToggleTerminal));
        let shell = tooltip(
            shell,
            container(text("Terminal in the jail").size(style::SMALL))
                .padding([4, 8])
                .style(style::fill(P.code_bg)),
            tooltip::Position::Top,
        );
        let mut bar = row![shell, space().width(Fill)]
            .spacing(4)
            .align_y(Alignment::Center);
        for control in controls(view) {
            bar = bar.push(control);
        }
        if let Some(usage) = usage(view) {
            bar =
                bar.push(container(text(usage).size(style::SMALL).color(P.fg_dim)).padding([0, 6]));
        }
        bar = bar.push(action);
        // as in the shelf, the prompt folds away while the terminal is up in its place
        let input: Element<'_, Message> = match (&view.terminal, view.terminal_shown) {
            (Some(pane), true) => self.terminal_pane(pane),
            _ => editor.into(),
        };
        composer = composer.push(input).push(container(bar).padding(Padding {
            top: 0.0,
            right: 10.0,
            bottom: 6.0,
            left: 10.0,
        }));
        column![
            rule::horizontal(1).style(style::rule_style(P.border)),
            container(composer)
                .width(Fill)
                .style(style::fill(P.surface)),
        ]
        .into()
    }

    /// The jail shell under its caption: a rule and label in the success colour, the counterpart
    /// of the host shell's warning-coloured one (the shelf's `sill-jail-caption`).
    fn terminal_pane<'a>(&'a self, pane: &'a crate::terminal::Pane) -> Element<'a, Message> {
        use crate::terminal::Status;
        let mut caption = row![
            text("jail · inside the session")
                .size(style::TINY)
                .color(P.success),
            space().width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let body: Element<'_, Message> = match &pane.status {
            Status::Running => {
                caption = caption.push(keys::press(
                    button(text("End shell").size(style::TINY))
                        .padding([0, 6])
                        .style(style::quiet),
                    Message::EndTerminal,
                ));
                pane.view().map(Message::TermAction)
            }
            Status::Opening => center(text("Opening the shell…").color(P.fg_dim)).into(),
            Status::Failed(error) => ended(format!("The shell could not be opened: {error}")),
            Status::Ended => ended("The shell ended.".into()),
        };
        column![
            rule::horizontal(1).style(style::rule_style(P.success)),
            container(caption).padding([2, 12]),
            container(body)
                .height(300)
                .width(Fill)
                .style(style::fill(P.bg)),
        ]
        .into()
    }
}

/// The selected session's lane state, for the dot's tooltip.
fn status_text(status: &Status) -> &'static str {
    match status {
        Status::Closed => "Not open",
        Status::Connecting => "Connecting…",
        Status::Reconnecting(_) => "Reconnecting…",
        Status::Loading => "Catching up…",
        Status::Live => "Connected",
        Status::NotRunning => "Not running",
        Status::Gone(_) => "Gone",
        Status::Stopped(_) => "Stopped following",
    }
}

fn placeholder(view: &SessionView) -> &'static str {
    match view.status {
        Status::NotRunning => "Not running — sending a prompt starts it",
        _ => "",
    }
}

/// The small round dot at the top bar's right, with what it means.
fn status_dot<'a>(color: Color, tip: String) -> Element<'a, Message> {
    tooltip(
        container(space().width(8).height(8)).style(move |_| container::Style {
            background: Some(color.into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        }),
        container(text(tip).size(style::TINY))
            .padding([3, 8])
            .style(style::fill(P.surface)),
        tooltip::Position::Left,
    )
    .into()
}

/// A tab's mark for its session's state, if it has one to show.
fn mark<'a>(attention: &Attention) -> Option<Element<'a, Message>> {
    let (glyph, color) = match attention.level() {
        Level::Question => ("?", P.warning),
        Level::Problem => ("!", P.error),
        Level::Working => ("●", P.accent),
        Level::Calm => return None,
    };
    Some(
        text(glyph)
            .size(if glyph == "●" {
                style::TINY
            } else {
                style::SMALL
            })
            .color(color)
            .into(),
    )
}

/// Stop the selected session's turn (found by its id in tests: it has no text).
fn stop_button<'a>() -> Element<'a, Message> {
    container(keys::press(
        button(
            svg(icons::stop())
                .width(14)
                .height(14)
                .style(style::icon(P.bg)),
        )
        .padding(6)
        .style(style::solid(P.error)),
        Message::Stop,
    ))
    .id("stop")
    .into()
}

fn option_style(
    kind: &str,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    if kind.starts_with("allow") {
        style::solid(P.accent)
    } else if kind.starts_with("reject") {
        style::solid(P.error)
    } else {
        style::solid(P.fg_dim)
    }
}

/// Prompts on their way, or that did not arrive: what happened, and what can be done.
fn outbox<'a>(mut composer: Column<'a, Message>, view: &'a SessionView) -> Column<'a, Message> {
    for pending in view.outbox.pending() {
        let id = pending.id().to_string();
        let (said, color) = match &pending.state {
            State::Sending => ("Sending…".to_string(), P.fg_dim),
            State::Checking => ("Checking whether it arrived…".to_string(), P.fg_dim),
            State::Failed(why) => (format!("Not sent: {why}"), P.error),
            State::Uncertain(why) => (format!("May not have arrived: {why}"), P.warning),
        };
        let mut line = row![
            container(
                column![
                    text(one_line(pending.text(), 100))
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::None),
                    text(one_line(&said, 110))
                        .size(style::TINY)
                        .color(color)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(2)
            )
            .width(Fill)
            .clip(true),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        let small = |label: &'static str, message: Message| {
            keys::press(
                button(text(label).size(style::TINY))
                    .padding([1, 6])
                    .style(style::quiet),
                message,
            )
        };
        match &pending.state {
            State::Failed(_) => {
                line = line
                    .push(small("Edit", Message::EditSend(id.clone())))
                    .push(small("Send again", Message::RetrySend(id.clone())))
                    .push(small("Discard", Message::DiscardSend(id)));
            }
            State::Uncertain(_) => {
                line = line
                    .push(small("Check", Message::CheckSend(id.clone())))
                    .push(small("Edit", Message::EditSend(id.clone())))
                    .push(small("Discard", Message::DiscardSend(id)));
            }
            State::Sending | State::Checking => {}
        }
        composer = composer.push(container(line).padding([3, 12]));
    }
    composer
}

/// The agent's ACP controls as dropdowns: its modes, then each of its options.
fn controls(view: &SessionView) -> Vec<Element<'_, Message>> {
    let Some(controls) = view
        .conversation
        .session
        .as_ref()
        .and_then(|s| s.controls.as_ref())
    else {
        return vec![];
    };
    let mut out = vec![];
    if !controls.modes.is_empty() {
        let picks: Vec<Pick> = controls
            .modes
            .iter()
            .map(|m| Pick {
                value: m.value.clone(),
                title: m.title.clone(),
                boolean: None,
            })
            .collect();
        let selected = controls
            .current_mode
            .as_ref()
            .and_then(|current| picks.iter().find(|p| &p.value == current).cloned());
        out.push(control(picks, selected, Message::SetMode));
    }
    for option in &controls.options {
        // the mode set already covers a "mode"-category option (Claude Code sends both; the
        // shelf skips it the same way)
        if option.category == "mode" && !controls.modes.is_empty() {
            continue;
        }
        let (picks, selected) = match &option.value {
            sill_proto::sfp::ControlValue::Boolean { value } => {
                let picks = vec![
                    Pick {
                        value: "false".into(),
                        title: "Off".into(),
                        boolean: Some(false),
                    },
                    Pick {
                        value: "true".into(),
                        title: "On".into(),
                        boolean: Some(true),
                    },
                ];
                let selected = picks.iter().find(|p| p.boolean == Some(*value)).cloned();
                (picks, selected)
            }
            sill_proto::sfp::ControlValue::Id { value } => {
                let picks: Vec<Pick> = option
                    .choices
                    .iter()
                    .map(|c| Pick {
                        value: c.value.clone(),
                        title: c.title.clone(),
                        boolean: None,
                    })
                    .collect();
                let selected = picks.iter().find(|p| &p.value == value).cloned();
                (picks, selected)
            }
        };
        let option_id = option.option_id.clone();
        out.push(control(picks, selected, move |pick| {
            Message::SetOption(option_id.clone(), pick)
        }));
    }
    out
}

/// Context used of the model's window, as the agent last reported it.
fn usage(view: &SessionView) -> Option<String> {
    let usage = view.conversation.session.as_ref()?.usage.as_ref()?;
    let used: u64 = usage.used.parse().ok()?;
    let size: u64 = usage.size.parse().ok().filter(|s| *s > 0)?;
    Some(format!("{}%", used * 100 / size))
}

fn plan(item: &Item) -> Element<'_, Message> {
    let mut steps = Column::new().spacing(2);
    for block in &item.content {
        let status = block
            .extra
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let (glyph, color) = match status {
            "completed" => ("✓", alpha(P.fg, 0.5)),
            "in_progress" => ("◐", P.accent),
            _ => ("○", alpha(P.fg, 0.4)),
        };
        steps = steps.push(
            row![
                text(glyph).size(style::SMALL).color(color).width(16),
                selection::text(block.text.as_deref().unwrap_or(""))
                    .size(style::SMALL)
                    .style(style::selectable_inherited),
            ]
            .spacing(6),
        );
    }
    ruled(
        2.0,
        alpha(P.fg, 0.18),
        container(steps).padding([4, 10]).width(Fill),
    )
}

/// One ACP control: as wide as its selected label, as the shelf's dropdowns are. iced's pick
/// list sizes its menu to the control, so a longer choice wraps in the open menu — a dropdown that
/// sizes the two apart is still to be written.
fn control<'a>(
    picks: Vec<Pick>,
    selected: Option<Pick>,
    on_select: impl Fn(Pick) -> Message + 'a,
) -> Element<'a, Message> {
    let label = selected.as_ref().map_or("", |p| p.title.as_str());
    let width = text_width(label, style::SMALL) + 12.0 + style::SMALL + 4.0;
    let on_select = std::rc::Rc::new(on_select);
    let select = on_select.clone();
    let choices = picks.clone();
    let current = selected.clone();
    let list = pick_list(picks, selected, move |pick| select(pick))
        .text_size(style::SMALL)
        .padding([3, 6])
        .width(width)
        .style(style::control)
        .menu_style(style::control_menu);
    // with the keyboard, Up and Down step through the choices (the menu is the pointer's)
    keys::focus(list)
        .on_key(move |key, _| {
            let step: isize = match key.as_ref() {
                Key::Named(Named::ArrowUp) => -1,
                Key::Named(Named::ArrowDown) => 1,
                _ => return None,
            };
            let at = current
                .as_ref()
                .and_then(|c| choices.iter().position(|p| p == c))
                .map_or(0, |i| i as isize);
            let next = (at + step).clamp(0, choices.len() as isize - 1) as usize;
            choices
                .get(next)
                .filter(|p| Some(*p) != current.as_ref())
                .map(|p| on_select(p.clone()))
        })
        .into()
}

/// Where the shell was: what became of it, and a fresh one.
fn ended<'a>(said: String) -> Element<'a, Message> {
    center(
        column![
            text(said).size(style::SMALL).color(P.fg_dim),
            keys::press(
                button(text("Open a new shell").size(style::SMALL))
                    .padding([4, 10])
                    .style(style::quiet),
                Message::ReopenTerminal
            ),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .into()
}

/// The width `content` takes in the prose face at `size`, laid out the way text widgets do.
fn text_width(content: &str, size: f32) -> f32 {
    use iced::advanced::text::Paragraph as _;
    let paragraph = iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
        content,
        bounds: iced::Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::default(),
        font: Font::with_name(crate::fonts::SANS_FAMILY),
        align_x: text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
    });
    paragraph.min_width().ceil()
}

/// Enter sends; Shift+Enter is a new line; everything else is the editor's own.
pub(crate) fn enter_sends(
    key_press: text_editor::KeyPress,
) -> Option<text_editor::Binding<Message>> {
    // the window's shortcuts work from the prompt, and PageUp/PageDown page the conversation
    if let Some(message) = keys::from_text(&key_press.key, key_press.modifiers, true) {
        return Some(text_editor::Binding::Custom(message));
    }
    if matches!(key_press.key, Key::Named(Named::Enter))
        && !key_press.modifiers.shift()
        && matches!(key_press.status, text_editor::Status::Focused { .. })
    {
        return Some(text_editor::Binding::Custom(Message::Send));
    }
    text_editor::Binding::from_key_press(key_press)
}

/// A read-only editor's keys: the window's shortcuts first, then the editor's own.
pub(crate) fn reading_keys(
    key_press: text_editor::KeyPress,
) -> Option<text_editor::Binding<Message>> {
    if let Some(message) = keys::from_text(&key_press.key, key_press.modifiers, false) {
        return Some(text_editor::Binding::Custom(message));
    }
    text_editor::Binding::from_key_press(key_press)
}

pub(crate) fn markdown_settings() -> markdown::Settings {
    let mut style = markdown::Style::from_palette(style::theme().palette());
    // iced's Markdown takes the generic sans-serif family, not the app's default font
    style.font = Font::with_name(crate::fonts::SANS_FAMILY);
    style.link_color = P.link;
    style.inline_code_color = P.fg;
    style.inline_code_font = MONO_FONT;
    style.code_block_font = MONO_FONT;
    style.inline_code_highlight = Highlight {
        background: P.code_bg.into(),
        border: iced::Border::default(),
    };
    let mut settings = markdown::Settings::with_text_size(style::TEXT, style);
    settings.h1_size = (style::TEXT * 1.45).into();
    settings.h2_size = (style::TEXT * 1.3).into();
    settings.h3_size = (style::TEXT * 1.15).into();
    settings.h4_size = (style::TEXT * 1.05).into();
    settings.h5_size = style::TEXT.into();
    settings.h6_size = style::TEXT.into();
    settings.code_size = style::MONO.into();
    settings.spacing = (style::TEXT * 0.8).into();
    settings
}

/// `content` behind a left rule `width` wide in `color`: how the shelf marks a block's kind.
fn ruled<'a>(
    width: f32,
    color: Color,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        rule::vertical(width).style(style::rule_style(color)),
        content.into()
    ]
    .height(Shrink)
    .into()
}

/// What a question asks, under its title: the description and the subject.
fn asked<'a>(mut card: Column<'a, Message>, question: &'a Question) -> Column<'a, Message> {
    if let Some(description) = &question.description {
        card = card.push(
            selection::text(description)
                .size(style::SMALL)
                .style(style::selectable_inherited),
        );
    }
    if let Some(subject) = &question.subject
        && let Some(text) = &subject.text
    {
        card = card.push(
            selection::text(text)
                .font(MONO_FONT)
                .size(style::MONO)
                .style(style::selectable(alpha(P.fg, 0.85))),
        );
    }
    card
}

/// An error or a notice: its colour as text, tint and rule (`.sill-error-note`).
fn note<'a>(message: &'a str, color: Color) -> Element<'a, Message> {
    ruled(
        3.0,
        color,
        container(
            selection::text(message)
                .size(style::SMALL)
                .style(style::selectable(color)),
        )
        .padding([6, 12])
        .width(Fill)
        .style(style::fill(alpha(color, 0.08))),
    )
}

/// A unified diff, coloured by line: additions, deletions, hunk headers.
fn diff(diff: &str) -> Element<'_, Message> {
    const LIMIT: usize = 400;
    let lines: Vec<&str> = diff.lines().collect();
    let mut column = Column::new();
    for line in lines.iter().take(LIMIT) {
        let color = if line.starts_with("+++") || line.starts_with("---") {
            P.fg_dim
        } else if line.starts_with('+') {
            P.success
        } else if line.starts_with('-') {
            P.error
        } else if line.starts_with("@@") {
            P.fg_dim
        } else {
            alpha(P.fg, 0.8)
        };
        column = column.push(
            selection::text(*line)
                .font(MONO_FONT)
                .size(style::MONO)
                .style(style::selectable(color)),
        );
    }
    if lines.len() > LIMIT {
        column = column.push(
            text(format!("… {} more lines", lines.len() - LIMIT))
                .size(style::TINY)
                .color(P.fg_dim),
        );
    }
    container(
        container(column)
            .padding([6, 10])
            .style(style::fill(P.code_bg)),
    )
    .padding(Padding {
        top: 2.0,
        right: 10.0,
        bottom: 6.0,
        left: 28.0,
    })
    .width(Fill)
    .into()
}

/// The shelf's glyph for an ACP tool kind (`shelf/conversation.rs` `kind_glyph`).
fn kind_glyph(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "read" => "›",
        "edit" => "✎",
        "delete" => "✕",
        "move" => "→",
        "search" => "⌕",
        "execute" => "$",
        "fetch" => "↓",
        "think" => "…",
        "delegate" => "⤷",
        _ => "⚙",
    }
}

/// The status mark at the end of a tool line, and its colour.
fn status_mark(status: &str) -> (&'static str, Color) {
    match status.to_ascii_lowercase().replace('_', "").as_str() {
        "completed" => ("✓", alpha(P.fg, 0.5)),
        "failed" => ("✗", P.error),
        "inprogress" => ("◌", P.accent),
        _ => ("•", alpha(P.fg, 0.4)),
    }
}

/// One line of at most `max` characters: newlines shown as ⏎, the rest cut with "…" (iced 0.14
/// text has no ellipsis of its own).
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let flat = text.trim().replace('\n', " ⏎ ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push('…');
    cut
}

/// The spinner's angle: one turn a second, from the time of the last tick.
fn spin(now: Instant) -> f32 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    now.saturating_duration_since(start).as_secs_f32() * std::f32::consts::TAU
}

/// A short arc going round (`shelf/spinner.rs`).
struct Spinner {
    angle: f32,
    color: Color,
}

impl canvas::Program<Message> for Spinner {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let center = frame.center();
        let radius = bounds.width.min(bounds.height) / 2.0 - 2.0;
        let arc = canvas::Path::new(|path| {
            path.arc(canvas::path::Arc {
                center,
                radius,
                start_angle: Radians(self.angle),
                end_angle: Radians(self.angle + 4.2),
            });
        });
        frame.stroke(
            &arc,
            canvas::Stroke::default()
                .with_width(2.0)
                .with_color(self.color),
        );
        vec![frame.into_geometry()]
    }
}
