//! The window as the shelf draws it (`docs/reference-ui` 01–02 and 19–22): a top bar with the
//! sill mark, one tab per running session and the agent's activity at the right; the
//! conversation; the composer with the agent's controls and send / stop.
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
use sill_proto::sfp::{Answer, Item, Question, Tool};

use crate::style::{self, P, alpha};
use crate::{App, Message, Pick, SessionChoice, icons, viewer};

const MONO_FONT: Font = Font::with_name(crate::fonts::MONO_FAMILY);

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let content = responsive(move |size| {
            let body = match self.file_view() {
                Some(view) => view,
                None => {
                    let conversation = match &self.text_view {
                        Some(content) => self.transcript_view(content),
                        None => self.feed(),
                    };
                    column![conversation, self.composer()].height(Fill).into()
                }
            };
            let main = column![self.file_chips(), body].width(Fill).height(Fill);
            if self.workspace.shown {
                if size.width < 600.0 {
                    return self.file_drawer().map(|message| match message {
                        Message::Workspace(crate::workspace::Action::Open { path, repo }) => {
                            Message::Workspace(crate::workspace::Action::OpenAndHide { path, repo })
                        }
                        message => message,
                    });
                }
                row![
                    main,
                    rule::vertical(1).style(style::rule_style(P.border)),
                    container(self.file_drawer()).width((size.width * 0.28).clamp(205.0, 320.0))
                ]
                .height(Fill)
                .into()
            } else {
                main.into()
            }
        });
        column![self.top_bar(), content].into()
    }

    // --- top bar --------------------------------------------------------------------------------

    fn top_bar(&self) -> Element<'_, Message> {
        let mark = svg(icons::herring())
            .width(18)
            .height(18)
            .style(style::icon(P.fg));
        let mut tabs = row![].spacing(2).height(Fill);
        for choice in &self.choices {
            tabs = tabs.push(self.tab(choice));
        }
        let select = tooltip(
            button(text("⌶").size(style::TEXT))
                .padding([2, 8])
                .style(style::quiet)
                .on_press(Message::ToggleText),
            container(text("Select text across messages").size(style::TINY))
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
                    space().width(Fill),
                    self.activity(),
                    select,
                    button(
                        text(if self.workspace.shown {
                            "›"
                        } else {
                            "Files ‹"
                        })
                        .size(style::SMALL)
                    )
                    .padding([2, 8])
                    .style(style::quiet)
                    .on_press(Message::Workspace(crate::workspace::Action::ToggleDrawer)),
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

    fn tab<'a>(&'a self, choice: &'a SessionChoice) -> Element<'a, Message> {
        let active = self.selected.as_ref().is_some_and(|s| s.id == choice.id);
        let color = if active { P.fg } else { alpha(P.fg, 0.6) };
        let label = button(
            row![
                svg(icons::for_agent(&choice.agent))
                    .width(13)
                    .height(13)
                    .style(style::icon(color)),
                text(&choice.title).size(style::TEXT).color(color),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([4, 12])
        .style(style::quiet)
        .on_press(Message::Choose(choice.clone()));
        let underline = rule::horizontal(2).style(style::rule_style(if active {
            P.accent
        } else {
            Color::TRANSPARENT
        }));
        column![
            container(label).height(Fill).align_y(Alignment::Center),
            underline
        ]
        .width(Shrink)
        .height(Fill)
        .into()
    }

    /// Right of the tabs: the timer and a spinner while the agent works, the spinner alone while
    /// a question waits, otherwise a dot — grey when all is well, red when disconnected.
    fn activity(&self) -> Element<'_, Message> {
        let activity = self
            .conversation
            .session
            .as_ref()
            .map_or("", |s| s.activity.as_str());
        let disconnected = self.error.is_some() && !self.demo;
        if !disconnected && (activity == "working" || activity == "waiting") {
            let mut parts = row![].spacing(8).align_y(Alignment::Center);
            if activity == "working"
                && let Some(elapsed) = self.elapsed()
            {
                parts = parts.push(
                    text(elapsed)
                        .size(style::TEXT)
                        .font(MONO_FONT)
                        .color(P.fg_dim),
                );
            }
            return parts
                .push(
                    canvas(Spinner {
                        angle: spin(self.now),
                        color: P.accent,
                    })
                    .width(16)
                    .height(16),
                )
                .into();
        }
        let (color, tip) = if disconnected {
            (P.error, self.error.clone().unwrap_or_default())
        } else {
            (alpha(P.fg, 0.35), self.status.clone())
        };
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

    /// Working time of the current turn, m:ss (completed intervals plus the running one).
    fn elapsed(&self) -> Option<String> {
        let session = self.conversation.session.as_ref()?;
        let since = session.active_since_ms?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        let seconds = (session.active_ms + now.saturating_sub(since)) / 1000;
        Some(format!("{}:{:02}", seconds / 60, seconds % 60))
    }

    // --- the conversation -----------------------------------------------------------------------

    fn feed(&self) -> Element<'_, Message> {
        let mut feed = Column::new().spacing(12).padding(Padding {
            top: 10.0,
            right: 14.0,
            bottom: 18.0,
            left: 12.0,
        });
        let questions: Vec<&Question> = self.conversation.questions.values().collect();
        let mut placed = BTreeSet::new();
        let items = self.conversation.visible_items();
        for (index, item) in items.iter().enumerate() {
            feed = feed.push(self.item(item));
            for question in questions
                .iter()
                .filter(|q| q.tool_item_id.as_deref() == Some(item.item_id.as_str()))
            {
                placed.insert(question.question_id.as_str());
                feed = feed.push(self.question(question));
            }
            // the last item of a turn that was stopped or failed: say so under it
            if let Some(turn_id) = &item.turn_id
                && items.get(index + 1).and_then(|next| next.turn_id.as_ref()) != Some(turn_id)
                && let Some(end) = self.conversation.turn_end(turn_id)
            {
                feed = feed.push(
                    container(text(format!("· {end}")).size(style::SMALL).color(P.fg_dim))
                        .padding([0, 8]),
                );
            }
        }
        for question in questions
            .iter()
            .filter(|q| !placed.contains(q.question_id.as_str()))
        {
            feed = feed.push(self.question(question));
        }
        if let Some(error) = &self.error {
            feed = feed.push(note(error, P.error));
        }
        if let Some(failed) = &self.failed {
            feed = feed.push(note(failed, P.error));
        }
        let empty = self.conversation.session_id.is_empty();
        if empty {
            let hint = if self.loading {
                "Loading conversation…"
            } else if self.choices.is_empty() {
                "No running session. Start one in the shelf; it shows here as a tab."
            } else {
                "Choose a session above."
            };
            return container(center(text(hint).size(style::TEXT).color(P.fg_dim)))
                .height(Fill)
                .into();
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
        if self.follow_tail {
            return scroll.into();
        }
        let latest = right(
            button(text("↓ Latest").size(style::SMALL))
                .padding([4, 10])
                .style(style::solid(P.surface))
                .on_press(Message::Latest),
        )
        .padding(Padding {
            top: 0.0,
            right: 18.0,
            bottom: 12.0,
            left: 0.0,
        })
        .align_y(Alignment::End)
        .height(Fill);
        stack![scroll, latest].into()
    }

    fn item<'a>(&'a self, item: &'a Item) -> Element<'a, Message> {
        if let Some(tool) = &item.tool {
            return self.tool(item, tool);
        }
        let source = self
            .rendered
            .get(&item.item_id)
            .map_or("", |r| r.source.as_str());
        match item.kind.as_str() {
            // the person's own words: a framed box on the code background behind the accent rule
            "user" => ruled(
                3.0,
                alpha(P.accent, 0.9),
                container(iced_selection::text(source).size(style::TEXT))
                    .padding([10, 14])
                    .width(Fill)
                    .style(style::fill(P.code_bg)),
            ),
            "thought" => container(
                iced_selection::text(source)
                    .size(style::SMALL)
                    .style(style::selectable(alpha(P.fg, 0.55))),
            )
            .padding([0, 8])
            .into(),
            "note" => container(
                text(format!("· {source}"))
                    .size(style::SMALL)
                    .color(P.fg_dim),
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
            "plan" => self.plan(item),
            // agent text and anything newer: Markdown as the agent wrote it
            _ => match self.rendered.get(&item.item_id) {
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

    fn plan<'a>(&'a self, item: &'a Item) -> Element<'a, Message> {
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
                    iced_selection::text(block.text.as_deref().unwrap_or("")).size(style::SMALL),
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

    /// A tool call: one quiet line — kind glyph, title, status mark — that opens to its output and
    /// diff (`.sill-tool`, `.sill-tool-line`, `.sill-tool-body`).
    fn tool<'a>(&'a self, item: &'a Item, tool: &'a Tool) -> Element<'a, Message> {
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
        .style(style::row_button)
        .on_press(Message::ToggleTool(item.item_id.clone()));
        let mut body = Column::new().push(line);
        if self.open_tools.contains(&item.item_id) {
            if !tool.detail.trim().is_empty() {
                body = body.push(
                    container(
                        iced_selection::text(tool.detail.trim_end())
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
    fn question<'a>(&'a self, question: &'a Question) -> Element<'a, Message> {
        let mut card = Column::new().spacing(8);
        if question.state == "open" {
            card = card.push(text(&question.title).size(style::TEXT));
            card = asked(card, question);
            if question.fields.is_empty() {
                let mut options = row![].spacing(8);
                for option in &question.options {
                    let paint = if option.kind.starts_with("allow") {
                        style::solid(P.accent)
                    } else if option.kind.starts_with("reject") {
                        style::solid(P.error)
                    } else {
                        style::solid(P.fg_dim)
                    };
                    options = options.push(
                        button(text(&option.title).size(style::TEXT))
                            .padding([6, 12])
                            .style(paint)
                            .on_press(Message::Answer {
                                question_id: question.question_id.clone(),
                                option_id: option.option_id.clone(),
                            }),
                    );
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
            let open = self.open_answers.contains(&question.question_id);
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
            card = card.push(text(format!("{} → {outcome}", question.title)).size(style::TEXT));
            card = card.push(
                button(text(if open { "▾ hide" } else { "▸ show" }).size(style::TINY))
                    .padding([0, 4])
                    .style(style::quiet)
                    .on_press(Message::ToggleAnswered(question.question_id.clone())),
            );
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
                button(text("Refresh").size(style::SMALL))
                    .style(style::quiet)
                    .on_press(Message::RefreshText),
                button(text("Copy all").size(style::SMALL))
                    .style(style::quiet)
                    .on_press_with(|| Message::Copy(self.conversation.transcript())),
                button(text("Close").size(style::SMALL))
                    .style(style::quiet)
                    .on_press(Message::ToggleText),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            text_editor(content)
                .on_action(Message::TextAction)
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

    fn composer(&self) -> Element<'_, Message> {
        let has_session = !self.conversation.session_id.is_empty();
        let mut composer = Column::new();
        for submission in self
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
                    button(text("×").size(style::SMALL))
                        .padding([0, 6])
                        .style(style::quiet)
                        .on_press(Message::Unqueue(submission.submission_id.clone())),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .padding([2, 12]),
            );
        }
        let editor = text_editor(&self.prompt)
            .id("prompt")
            .on_action(Message::Prompt)
            .key_binding(enter_sends)
            .padding([8, 14])
            .size(style::TEXT)
            .height(Shrink)
            .max_height(220)
            .style(style::prompt);
        let working = self
            .conversation
            .session
            .as_ref()
            .is_some_and(|s| s.activity == "working" || s.activity == "waiting");
        let action: Element<'_, Message> = if working && has_session {
            button(
                svg(icons::stop())
                    .width(14)
                    .height(14)
                    .style(style::icon(P.bg)),
            )
            .padding(6)
            .style(style::solid(P.error))
            .on_press(Message::Stop)
            .into()
        } else {
            button(
                svg(icons::send())
                    .width(14)
                    .height(14)
                    .style(style::icon(P.bg)),
            )
            .padding(6)
            .style(style::solid(P.accent))
            .on_press_maybe(
                (has_session && !self.prompt.text().trim().is_empty()).then_some(Message::Send),
            )
            .into()
        };
        let shell = button(text(">_").size(style::SMALL).font(MONO_FONT).color(
            if self.terminal_shown {
                P.accent
            } else {
                P.fg_dim
            },
        ))
        .padding([2, 6])
        .style(style::quiet)
        .on_press_maybe(has_session.then_some(Message::ToggleTerminal));
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
        for control in self.controls() {
            bar = bar.push(control);
        }
        if let Some(usage) = self.usage() {
            bar =
                bar.push(container(text(usage).size(style::SMALL).color(P.fg_dim)).padding([0, 6]));
        }
        bar = bar.push(action);
        // as in the shelf, the prompt folds away while the terminal is up in its place
        let input: Element<'_, Message> = match (&self.terminal, self.terminal_shown) {
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
                caption = caption.push(
                    button(text("End shell").size(style::TINY))
                        .padding([0, 6])
                        .style(style::quiet)
                        .on_press(Message::EndTerminal),
                );
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

    /// The agent's ACP controls as dropdowns: its modes, then each of its options.
    fn controls(&self) -> Vec<Element<'_, Message>> {
        let Some(controls) = self
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
    fn usage(&self) -> Option<String> {
        let usage = self.conversation.session.as_ref()?.usage.as_ref()?;
        let used: u64 = usage.used.parse().ok()?;
        let size: u64 = usage.size.parse().ok().filter(|s| *s > 0)?;
        Some(format!("{}%", used * 100 / size))
    }
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
    pick_list(picks, selected, on_select)
        .text_size(style::SMALL)
        .padding([3, 6])
        .width(width)
        .style(style::control)
        .menu_style(style::control_menu)
        .into()
}

/// The width `content` takes in the prose face at `size`, laid out the way text widgets do.
/// Where the shell was: what became of it, and a fresh one.
fn ended<'a>(said: String) -> Element<'a, Message> {
    center(
        column![
            text(said).size(style::SMALL).color(P.fg_dim),
            button(text("Open a new shell").size(style::SMALL))
                .padding([4, 10])
                .style(style::quiet)
                .on_press(Message::ReopenTerminal),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .into()
}

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
    if matches!(key_press.key, Key::Named(Named::Enter))
        && !key_press.modifiers.shift()
        && matches!(key_press.status, text_editor::Status::Focused { .. })
    {
        return Some(text_editor::Binding::Custom(Message::Send));
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
        card = card.push(iced_selection::text(description).size(style::SMALL));
    }
    if let Some(subject) = &question.subject
        && let Some(text) = &subject.text
    {
        card = card.push(
            iced_selection::text(text)
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
            iced_selection::text(message)
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
            iced_selection::text(*line)
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
fn one_line(text: &str, max: usize) -> String {
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
