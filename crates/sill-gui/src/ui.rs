//! The window as the shelf draws it (`docs/reference-ui` 01–02 and 19–22): a top bar with the
//! sill mark, one tab per open session and the selected agent's activity at the right; the
//! conversation; the composer with the agent's controls and send / stop.
//!
//! Supervision (docs/sill-gui.md, direction 4): every tab carries its session's mark — working,
//! a question waiting, a problem.
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

use crate::layout::{self, Drawer};
use crate::style::{self, P, alpha};
use crate::{App, Message, Pick, SessionView, Tab, icons, keys, rows, selection, viewer};

const MONO_FONT: Font = Font::with_name(crate::fonts::MONO_FAMILY);

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let content = responsive(move |size| {
            // The sill pages take the conversation's place.
            let page = match (&self.changes, &self.settings_page) {
                (Some(page), _) => Some(self.changes_view(page)),
                (None, Some(page)) => Some(self.settings_view(page)),
                _ => None,
            };
            if let Some(page) = page {
                return column![page].height(Fill).into();
            }
            let Some(view) = self.current() else {
                return self.empty();
            };
            let body = match self.file_view(view) {
                Some(reader) => reader,
                None => column![self.feed(view), self.composer(view)]
                    .height(Fill)
                    .into(),
            };
            // a file open takes the chips' row too, as in the shelf
            let chips: Element<'_, Message> = if view.files.reader.is_some() {
                space().height(0).into()
            } else {
                self.file_chips(view)
            };
            let main = column![chips, body].width(Fill).height(Fill);
            let placement = self.layout.drawer(self.workspace.shown, size.width);
            // the conversation keeps one place in the widget tree wherever the drawer is: moving
            // it would give its scrollable fresh state (back at the top) and drop a selection
            let mut beside = row![main].height(Fill);
            if let Drawer::Beside(width) = placement {
                beside = beside
                    .push(self.drawer_grip())
                    .push(container(self.file_drawer(view)).width(width));
            }
            let mut page = stack![beside];
            if let Drawer::Over(width) = placement {
                // A file opened from the overlay takes the conversation's place.
                let drawer = self.file_drawer(view).map(|message| match message {
                    Message::Workspace(crate::workspace::Action::Open { path, repo }) => {
                        Message::Workspace(crate::workspace::Action::OpenAndHide { path, repo })
                    }
                    message => message,
                });
                page = page.push(row![
                    mouse_area(
                        container(space())
                            .width(Fill)
                            .height(Fill)
                            .style(style::fill(alpha(Color::BLACK, 0.25)))
                    )
                    .on_press(Message::Workspace(crate::workspace::Action::ToggleDrawer)),
                    self.drawer_grip(),
                    container(drawer)
                        .width(width)
                        .height(Fill)
                        .style(style::fill(P.bg)),
                ]);
            }
            column![page.height(Fill)].height(Fill).into()
        });
        let bar = container(responsive(move |size| self.top_bar(size.width))).height(37);
        let window = column![bar, content];
        // Keep the conversation at the same tree position when the menu opens/closes.
        // Wrapping it in a stack only while open discards its scroll and selection state.
        let mut layers = stack![window];
        // GTK's corner chevron borrows the drawer's space without adding a row.
        if self.workspace.shown && !self.backend_shown() && self.current().is_some() {
            layers = layers.push(
                container(keys::press(
                    button(
                        container(icons::glyph("chevron-right", 14.0, alpha(P.fg, 0.55)))
                            .height(22)
                            .align_y(Alignment::Center),
                    )
                    .padding(2)
                    .style(style::quiet),
                    Message::Layout(layout::Action::Mode(layout::Mode::Full)),
                ))
                .id("expand-full")
                .align_right(Fill)
                .align_bottom(Fill)
                .padding(Padding {
                    right: 4.0,
                    bottom: 8.0,
                    ..Padding::ZERO
                }),
            );
        }
        if let Some(menu) = &self.menu {
            layers = layers.push(self.session_menu(menu));
        }
        layers.into()
    }

    /// The drawer's edge: a line to drag, wider than it looks.
    fn drawer_grip(&self) -> Element<'_, Message> {
        let line = if self.layout.dragging {
            P.accent
        } else {
            P.border
        };
        mouse_area(
            container(rule::vertical(1).style(style::rule_style(line)))
                .center_x(layout::HANDLE)
                .height(Fill)
                .style(style::fill(P.bg)),
        )
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .on_press(Message::Layout(layout::Action::DragStart))
        .into()
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

    fn top_bar(&self, width: f32) -> Element<'_, Message> {
        // the herring opens the sill pages, underlined while one shows
        let shown = self.changes.is_some() || self.settings_page.is_some();
        let drawer_open = self.workspace.shown && !shown && self.current().is_some();
        // room on its right, as the shelf's status slot has (`.sill-status-slot`)
        let activity = container(self.activity()).padding(Padding {
            left: 2.0,
            right: 8.0,
            ..Padding::ZERO
        });
        let mark = tooltip(
            keys::press(
                // a box just round the mark, its underline as wide as the box (as a tab's)
                button(
                    column![
                        container(crate::herring::herring(18.0, P.fg, self.herring_flick))
                            .center(Fill),
                        rule::horizontal(2).style(style::rule_style(if shown {
                            P.accent
                        } else {
                            Color::TRANSPARENT
                        })),
                    ]
                    .width(Fill)
                    .height(Fill),
                )
                .padding(0)
                .width(34)
                .height(Fill)
                .style(style::quiet),
                Message::ToggleChanges,
            ),
            container(
                text(if shown {
                    "Back to the conversation"
                } else {
                    "Sill: Changes and Settings"
                })
                .size(style::TINY),
            )
            .padding([3, 8])
            .style(style::fill(P.surface)),
            tooltip::Position::Bottom,
        );
        let mut tabs = row![].spacing(2).height(Fill);
        if shown {
            // the pages' own tabs in place of the sessions', as the shelf's backend
            for (page, glyph, title) in [
                (crate::Page::Changes, "changes", "Changes"),
                (crate::Page::Settings, "settings", "Settings"),
            ] {
                tabs = tabs.push(self.page_tab(page, glyph, title));
            }
        } else {
            for tab in &self.tabs {
                tabs = tabs.push(self.tab(tab));
            }
        }
        // the window has no title bar: a press here that no tab or button takes moves it
        mouse_area(
            container(
                row![
                    container(mark).padding(Padding::from([0.0, 2.0])),
                    scrollable(tabs)
                        .width(Fill)
                        .height(Fill)
                        .direction(scrollable::Direction::Horizontal(style::bar()))
                        .style(style::feed_scroll),
                    // Keep the timer open across the gap and + button, so the button does
                    // not move away as the pointer approaches it from the indicator.
                    mouse_area(
                        row![
                            tooltip(
                                keys::press(
                                    button(
                                        svg(icons::plus())
                                            .width(15)
                                            .height(15)
                                            .style(style::icon(alpha(P.fg, 0.75))),
                                    )
                                    .padding(4)
                                    .style(style::quiet),
                                    Message::Menu(crate::menu::Event::Toggle),
                                ),
                                container(
                                    text("New session, reopen one, or change the view")
                                        .size(style::TINY)
                                )
                                .padding([3, 8])
                                .style(style::fill(P.surface)),
                                tooltip::Position::Bottom,
                            ),
                            // the activity over the drawer when it is beside the conversation, so the +
                            // ends the conversation's side, as in the shelf
                            match self.layout.drawer(drawer_open, width) {
                                // reaching back across the bar's spacing and the +'s padding, so the +
                                // sits about 10 px left of the divider (the shelf's place)
                                Drawer::Beside(drawer) => container(activity)
                                    .width(drawer + layout::HANDLE - 14.0)
                                    .align_right(drawer + layout::HANDLE - 14.0),
                                _ => container(activity),
                            },
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    )
                    .on_exit(Message::ActivityHover(false)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .height(37)
            .padding(Padding {
                top: 0.0,
                right: 6.0,
                bottom: 0.0,
                left: 0.0,
            })
            .width(Fill)
            .style(style::fill(P.bg)),
        )
        .on_press(Message::DragWindow)
        .into()
    }

    /// A page's tab: its glyph and title, underlined while it shows.
    fn page_tab<'a>(
        &'a self,
        page: crate::Page,
        glyph: &'static str,
        title: &'static str,
    ) -> Element<'a, Message> {
        let active = match page {
            crate::Page::Changes => self.changes.is_some(),
            crate::Page::Settings => self.settings_page.is_some(),
        };
        let color = if active { P.fg } else { alpha(P.fg, 0.6) };
        let label = keys::press(
            button(
                row![
                    icons::glyph(glyph, 14.0, color),
                    text(title).size(style::TEXT).color(color),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([4, 12])
            .style(style::quiet),
            Message::ShowPage(page),
        );
        column![
            container(label).height(Fill).align_y(Alignment::Center),
            rule::horizontal(2).style(style::rule_style(if active {
                P.accent
            } else {
                Color::TRANSPARENT
            })),
        ]
        .width(Shrink)
        .height(Fill)
        .into()
    }

    /// A tab: the agent glyph pulses in accent while working; question/problem marks follow
    /// the title. Dimmer when the session is not running.
    fn tab<'a>(&'a self, tab: &'a Tab) -> Element<'a, Message> {
        let active = self.selected.as_deref() == Some(tab.id.as_str());
        let view = self.views.get(&tab.id);
        let live = view.is_some_and(|v| !v.status.ended()) || tab.running;
        let color = match (active, live) {
            (true, _) => P.fg,
            (false, true) => alpha(P.fg, 0.6),
            (false, false) => alpha(P.fg, 0.35),
        };
        let attention = view.map(SessionView::attention);
        let working = attention
            .as_ref()
            .is_some_and(|a| a.activity == Activity::Working);
        let mut label = row![
            icons::tab_agent(&tab.agent, if working { P.accent } else { color }, working),
            text(&tab.title).size(style::TEXT).color(color),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(mark) = attention.as_ref().and_then(mark) {
            label = label.push(mark);
        }
        let label = keys::press(
            button(label).padding([4, 12]).style(style::quiet),
            Message::Choose(tab.id.clone()),
        );
        // the selected tab's ×: closing ends the session (a ghost's asks once: it deletes it)
        let label: Element<'a, Message> = if active && !self.demo {
            let armed = self.armed_close.as_deref() == Some(tab.id.as_str());
            row![
                label,
                tooltip(
                    container(keys::press(
                        button(if armed {
                            Element::from(text("Close?").size(style::SMALL).color(P.error))
                        } else {
                            icons::glyph("x", 12.0, alpha(P.fg, 0.55)).into()
                        })
                        .padding([4, 6])
                        .style(style::quiet),
                        Message::CloseTab(tab.id.clone()),
                    ))
                    .id("close-tab"),
                    container(
                        text(if armed {
                            "A ghost: closing deletes it and its unkept changes — press again"
                        } else {
                            "Close: ends the session (it can be reopened from +)"
                        })
                        .size(style::TINY),
                    )
                    .padding([3, 8])
                    .style(style::fill(P.surface)),
                    tooltip::Position::Bottom,
                ),
            ]
            .align_y(Alignment::Center)
            .into()
        } else {
            label
        };
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
        let busy_now = trouble.is_none()
            && matches!(attention.activity, Activity::Working | Activity::Waiting);
        let indicator = if busy_now {
            Indicator::Spinner(P.accent)
        } else {
            Indicator::Dot(match &trouble {
                Some(_) => P.error,
                None => alpha(P.fg, 0.35),
            })
        };
        // the timer, as the shelf's: while the agent works, and whenever the pointer is over the
        // activity — the session's worked time, still while it does not work; it slides out from
        // under the indicator when it appears, as the shelf's revealer does
        let working = attention
            .working_ms
            .filter(|_| attention.activity == Activity::Working);
        // Settings → Timer autoshow (the demo shows it)
        let auto = self
            .daemon_settings
            .as_ref()
            .and_then(|s| s.timer_auto)
            .unwrap_or(self.demo);
        let shown_ms = if self.activity_hovered {
            attention.worked_ms
        } else {
            working.filter(|_| auto)
        };
        let clock = shown_ms.map(|ms| {
            let shown = view.clock_shown.get().unwrap_or_else(Instant::now);
            view.clock_shown.set(Some(shown));
            let now = Instant::now();
            Clock {
                base: now.checked_sub(Duration::from_millis(ms)).unwrap_or(now),
                ticking: attention.working_ms.is_some(),
                ms,
                shown,
                width: text_width(&clock(ms).replace(char::is_numeric, "0"), style::TEXT)
                    .max(text_width("00:00", style::TEXT))
                    + 2.0,
            }
        });
        if clock.is_none() {
            view.clock_shown.set(None);
        }
        let shown = activity_canvas(indicator, clock);
        let shown: Element<'_, Message> = if busy_now {
            shown
        } else {
            let tip = trouble.unwrap_or_else(|| status_text(&view.status).into());
            tooltip(
                shown,
                container(text(tip).size(style::TINY))
                    .padding([3, 8])
                    .style(style::fill(P.surface)),
                tooltip::Position::Bottom,
            )
            .into()
        };
        mouse_area(shown)
            .on_enter(Message::ActivityHover(true))
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
        // the layout every row height depends on: the window's width, the drawer beside the
        // conversation, the reading measure
        let drawer = match self.layout.drawer(self.workspace.shown, self.width) {
            Drawer::Beside(width) => width,
            _ => 0.0,
        };
        let layout_key = rows::signature(
            (
                self.width.to_bits(),
                drawer.to_bits(),
                self.layout.measure().map(f32::to_bits),
            ),
            0,
        );
        let items = conversation.visible_items();
        // each question under its tool's item, or the item it was asked after
        let places = conversation.question_places();
        let below = |item: &Item| -> Vec<&'a Question> {
            places
                .get(item.item_id.as_str())
                .cloned()
                .unwrap_or_default()
        };
        // the last item of a turn that was stopped or failed: say so under it
        let turn_end = |index: usize, item: &Item| -> Option<String> {
            let turn_id = item.turn_id.as_ref()?;
            if items.get(index + 1).and_then(|next| next.turn_id.as_ref()) == Some(turn_id) {
                return None;
            }
            conversation.turn_end(turn_id).map(|end| end.to_string())
        };
        // what each row's height depends on, so a measured height is used only while it holds
        let signatures: Vec<u64> = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let source = view.rendered.get(&item.item_id).map(|r| r.source.as_str());
                let tool = item.tool.as_ref().map(|t| {
                    (
                        &t.title,
                        &t.status,
                        t.detail.len(),
                        t.diff.len(),
                        view.open_tools.contains(&item.item_id),
                    )
                });
                let asked: Vec<_> = below(item)
                    .into_iter()
                    .map(|q| {
                        (
                            &q.question_id,
                            &q.state,
                            &q.answer_id,
                            view.answering.contains(&q.question_id),
                            view.open_answers.contains(&q.question_id),
                            format!("{:?}", view.forms.get(&q.question_id)),
                        )
                    })
                    .collect();
                rows::signature(
                    (
                        &item.item_id,
                        &item.kind,
                        source,
                        tool,
                        asked,
                        turn_end(index, item),
                    ),
                    layout_key,
                )
            })
            .collect();
        let known: Vec<Option<f32>> = {
            let heights = view.heights.borrow();
            items
                .iter()
                .zip(&signatures)
                .map(|(item, signature)| {
                    heights
                        .get(&item.item_id)
                        .filter(|(s, _)| s == signature)
                        .map(|(_, h)| *h)
                })
                .collect()
        };
        const SPACING: f32 = 12.0;
        // until the conversation has scrolled, the window's height stands in for the view's (a
        // screen of rows to spare on each side covers the difference)
        let viewport = if view.viewport > 0.0 {
            view.viewport
        } else {
            self.height
        };
        let slots = rows::plan(
            &known,
            SPACING,
            view.scroll_offset,
            viewport,
            view.follow_tail,
        );
        let mut feed = iced::widget::keyed_column(std::iter::empty())
            .spacing(SPACING)
            .padding(Padding {
                top: 10.0,
                right: 14.0,
                bottom: 18.0,
                left: 12.0,
            });
        let mut placed = BTreeSet::new();
        for slot in slots {
            match slot {
                rows::Slot::Gap { first, height } => {
                    let gap_key = format!("gap {}", items[first].item_id);
                    feed = feed.push(rows::key(&gap_key), space().height(height));
                }
                rows::Slot::Row(index) => {
                    let item = items[index];
                    let mut row = Column::new().spacing(SPACING).push(self.item(view, item));
                    for question in below(item) {
                        row = row.push(self.question(view, question));
                    }
                    if let Some(end) = turn_end(index, item) {
                        row = row.push(
                            container(
                                selection::text(format!("· {end}"))
                                    .size(style::SMALL)
                                    .style(style::selectable(P.fg_dim)),
                            )
                            .padding([0, 8]),
                        );
                    }
                    feed = feed.push(
                        rows::key(&item.item_id),
                        rows::measured(row, &item.item_id, signatures[index], &view.heights),
                    );
                }
            }
        }
        // questions below items left out are placed by their item; the rest go at the end
        for question in places.values().flatten() {
            placed.insert(question.question_id.as_str());
        }
        for question in conversation
            .questions
            .values()
            .filter(|q| !placed.contains(q.question_id.as_str()))
        {
            feed = feed.push(
                rows::key(&format!("question {}", question.question_id)),
                self.question(view, question),
            );
        }
        if let Some(error) = view.status.problem() {
            feed = feed.push(rows::key("problem"), note(error, P.error));
        }
        if let Some(failed) = &view.failed {
            feed = feed.push(rows::key("failed"), note(failed, P.error));
        }
        let feed: Element<'a, Message> = match self.layout.measure() {
            Some(measure) => container(container(feed).max_width(measure))
                .center_x(Fill)
                .into(),
            None => feed.into(),
        };
        let scroll = scrollable(feed)
            .direction(scrollable::Direction::Vertical(style::bar()))
            .id("conversation")
            .height(Fill)
            .width(Fill)
            .style(style::feed_scroll)
            .on_scroll(|viewport| {
                Message::Scrolled(
                    viewport.relative_offset().y >= 0.99,
                    viewport.absolute_offset().y,
                    viewport.bounds().height,
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
                status_mark(&tool.status),
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
                card = card.push(form(view, question));
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

    // --- the composer ---------------------------------------------------------------------------

    fn composer<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let has_session = !view.conversation.session_id.is_empty() || self.demo;
        let mut composer = Column::new();
        for submission in view
            .conversation
            .submissions
            .values()
            .filter(|s| s.held && s.outcome == "pending")
        {
            composer = composer.push(held(submission));
        }
        let mut queued = Column::new().spacing(4);
        let mut has_queued = false;
        // in the order the daemon sends them: as accepted
        let mut waiting: Vec<_> = view
            .conversation
            .submissions
            .values()
            .filter(|s| {
                s.delivery == "queued"
                    && s.insertion == "pending"
                    && s.outcome == "pending"
                    && !s.held
            })
            .collect();
        waiting.sort_by_key(|s| s.accepted_seq.parse::<u64>().unwrap_or(u64::MAX));
        for submission in waiting {
            has_queued = true;
            queued = queued.push(
                row![
                    text("Queued").size(style::TINY).color(P.fg_dim),
                    container(
                        text(one_line(&submission.text, 120))
                            .size(style::SMALL)
                            .wrapping(text::Wrapping::None),
                    )
                    .width(Fill)
                    .clip(true),
                    tooltip(
                        keys::press(
                            button(
                                svg(icons::send())
                                    .width(11)
                                    .height(11)
                                    .style(style::icon(alpha(P.fg, 0.6))),
                            )
                            .padding([3, 6])
                            .style(style::quiet),
                            Message::SendNow(submission.submission_id.clone())
                        ),
                        container(
                            text("Send now: stops the agent's turn and sends this first")
                                .size(style::TINY),
                        )
                        .padding([3, 8])
                        .style(style::fill(P.surface)),
                        tooltip::Position::Top,
                    ),
                    keys::press(
                        button(icons::glyph("x", 12.0, alpha(P.fg, 0.6)))
                            .padding([2, 6])
                            .style(style::quiet),
                        Message::Unqueue(submission.submission_id.clone())
                    ),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .padding([2, 12]),
            );
        }
        if has_queued {
            composer = composer.push(column![
                container(queued)
                    .padding([6, 0])
                    .width(Fill)
                    .style(style::fill(alpha(P.accent, 0.05))),
                rule::horizontal(1).style(style::rule_style(P.border)),
                space().height(4),
            ]);
        }
        composer = outbox(composer, view);
        // what waits above the prompt is one child, however many rows it has: the prompt keeps
        // its place in the tree, and with it its focus (a first send's "Sending…" row moved it,
        // and the cursor left the box)
        let mut composer = Column::new().push(composer);
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
        let sendable = !view.draft.text().trim().is_empty();
        let action: Element<'_, Message> = if view.conversation.busy() {
            stop_button()
        } else {
            keys::press_maybe(
                button(
                    svg(icons::send())
                        .width(12)
                        .height(12)
                        .style(move |theme, status| {
                            // dark on the accent only when the button lights up: there is a prompt
                            if sendable {
                                style::send_arrow(theme, status)
                            } else {
                                style::send_arrow(theme, svg::Status::Idle)
                            }
                        }),
                )
                .padding(4)
                .style(style::send),
                sendable.then_some(Message::Send),
            )
        };
        let shell = button(icons::glyph(
            "terminal",
            12.0,
            if view.terminal_shown {
                P.accent
            } else {
                P.fg_dim
            },
        ))
        .padding([4, 6])
        .style(style::quiet);
        let shell = container(keys::press_maybe(
            shell,
            has_session.then_some(Message::ToggleTerminal),
        ))
        .id("terminal-button");
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
                bar.push(container(text(usage).size(style::TINY).color(P.fg_dim)).padding([0, 6]));
        }
        bar = bar.push(action);
        // as in the shelf, the prompt folds away while the terminal is up in its place
        let terminal = view.terminal.as_ref().filter(|_| view.terminal_shown);
        let input: Element<'_, Message> = match terminal {
            Some(pane) => self.terminal_pane(pane),
            None => editor.into(),
        };
        // under the prompt the icons sit low (the prompt's own padding is above them); under the
        // terminal the bar is a strip of its own, the icons centred in it — the same height
        let (top, bottom) = if terminal.is_some() {
            (3.0, 3.0)
        } else {
            (4.0, 2.0)
        };
        composer = composer.push(input).push(container(bar).padding(Padding {
            top,
            right: 10.0,
            bottom,
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
        Level::Working | Level::Calm => return None,
    };
    Some(text(glyph).size(style::SMALL).color(color).into())
}

/// Stop the selected session's turn (found by its id in tests: it has no text).
fn stop_button<'a>() -> Element<'a, Message> {
    container(keys::press(
        button(
            svg(icons::stop())
                .width(12)
                .height(12)
                .style(style::icon(P.error)),
        )
        .padding(4)
        .style(style::tinted(P.error)),
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

/// A form question's fields — choices as marks to press, text and numbers as fields — with
/// Submit and Decline. Values are encoded as the shelf sends them (`forms.rs`).
fn form<'a>(view: &'a SessionView, question: &'a Question) -> Element<'a, Message> {
    let entries = view.forms.get(&question.question_id);
    let sending = view.answering.contains(&question.question_id);
    let id = || question.question_id.clone();
    let mut fields = Column::new().spacing(10);
    for field in &question.fields {
        let entry = entries.and_then(|e| e.get(&field.key));
        let mut block = Column::new().spacing(4);
        block = block.push(text(crate::forms::title(field)).size(style::SMALL));
        if !field.description.is_empty() {
            block = block.push(
                selection::text(&field.description)
                    .size(style::TINY)
                    .style(style::selectable(P.fg_dim)),
            );
        }
        let edit = |edit: crate::forms::Edit| Message::FormEdit(id(), edit);
        let input: Element<'a, Message> = match field.kind.as_str() {
            "choice" | "choices" => {
                let single = field.kind == "choice";
                let mut choices = Column::new().spacing(2);
                for choice in &field.choices {
                    let picked = entry.is_some_and(|e| e.picked.contains(&choice.value));
                    let mark = match (single, picked) {
                        (true, true) => "◉",
                        (true, false) => "○",
                        (false, true) => "☑",
                        (false, false) => "☐",
                    };
                    let label = if choice.title.is_empty() {
                        &choice.value
                    } else {
                        &choice.title
                    };
                    choices = choices.push(keys::press(
                        button(
                            row![
                                text(mark).size(style::SMALL).color(if picked {
                                    P.accent
                                } else {
                                    P.fg_dim
                                }),
                                text(label).size(style::SMALL),
                            ]
                            .spacing(8),
                        )
                        .padding([2, 6])
                        .style(style::row_button),
                        edit(crate::forms::Edit::Pick {
                            key: field.key.clone(),
                            value: choice.value.clone(),
                        }),
                    ));
                }
                choices.into()
            }
            "boolean" => {
                let on = entry.is_some_and(|e| e.on);
                keys::press(
                    button(
                        row![
                            text(if on { "☑" } else { "☐" })
                                .size(style::SMALL)
                                .color(if on { P.accent } else { P.fg_dim }),
                            text("yes").size(style::SMALL),
                        ]
                        .spacing(8),
                    )
                    .padding([2, 6])
                    .style(style::row_button),
                    edit(crate::forms::Edit::Toggle {
                        key: field.key.clone(),
                    }),
                )
            }
            "text" | "number" => {
                let key = field.key.clone();
                iced::widget::text_input(
                    if field.kind == "number" {
                        "a number"
                    } else {
                        "optional"
                    },
                    entry.map_or("", |e| e.text.as_str()),
                )
                .size(style::SMALL)
                .padding(6)
                .on_input(move |value| {
                    Message::FormEdit(
                        id(),
                        crate::forms::Edit::Text {
                            key: key.clone(),
                            text: value,
                        },
                    )
                })
                .into()
            }
            other => text(format!("A {other} field cannot be answered here."))
                .size(style::TINY)
                .color(P.fg_dim)
                .into(),
        };
        block = block.push(input);
        fields = fields.push(block);
    }
    let answerable = crate::forms::answerable(question);
    let buttons = row![
        keys::press_maybe(
            button(text("Submit").size(style::TEXT))
                .padding([6, 12])
                .style(style::solid(P.accent)),
            (answerable && !sending).then(|| Message::SubmitForm(id())),
        ),
        keys::press_maybe(
            button(text("Decline").size(style::TEXT))
                .padding([6, 12])
                .style(style::solid(P.fg_dim)),
            (!sending).then(|| Message::DeclineForm(id())),
        ),
    ]
    .spacing(8);
    column![fields, buttons].spacing(12).into()
}

/// A prompt the daemon holds until the person decides (after a restart or a failure): send it
/// after all, or drop it.
fn held(submission: &sill_proto::sfp::Submission) -> Element<'_, Message> {
    let why = submission.hold_reason.as_ref().map_or_else(
        || "held until you decide".to_string(),
        |r| r.message.clone(),
    );
    let small = |label: &'static str, message: Message| {
        keys::press(
            button(text(label).size(style::TINY))
                .padding([1, 6])
                .style(style::quiet),
            message,
        )
    };
    container(
        row![
            container(
                column![
                    text(one_line(&submission.text, 100))
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::None),
                    text(one_line(&format!("Held: {why}"), 110))
                        .size(style::TINY)
                        .color(P.warning)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(2)
            )
            .width(Fill)
            .clip(true),
            small(
                "Send",
                Message::ResumeHeld(submission.submission_id.clone())
            ),
            small(
                "Discard",
                Message::DiscardHeld(submission.submission_id.clone())
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([3, 12])
    .into()
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

/// The room a control keeps at its right for its caret.
const CARET_ROOM: f32 = 19.0;

/// One ACP control: as wide as its selected label, as the shelf's dropdowns are. iced's pick
/// list sizes its menu to the control, so a longer choice wraps in the open menu — a dropdown that
/// sizes the two apart is still to be written.
/// Room on each side of a choice in an agent control's menu.
const MENU_SIDE: f32 = 14.0;

fn control<'a>(
    picks: Vec<Pick>,
    selected: Option<Pick>,
    on_select: impl Fn(Pick) -> Message + 'a,
) -> Element<'a, Message> {
    let label = selected.as_ref().map_or("", |p| p.title.as_str());
    // the shelf's controls are 80 % of the body text (`.sill-controls`); room at the right for
    // the caret
    let width = text_width(label, style::TINY) + 6.0 + CARET_ROOM;
    let on_select = std::rc::Rc::new(on_select);
    let select = on_select.clone();
    let choices = picks.clone();
    let current = selected.clone();
    // the menu reads as a menu, not the label's strip: as wide as its longest choice, roomier
    // rows in a little larger text, set apart from the control
    let menu_width = picks
        .iter()
        .map(|p| text_width(&p.title, style::SMALL))
        .fold(0.0, f32::max)
        + 2.0 * MENU_SIDE
        + 2.0;
    let list = pick_list(picks, selected, move |pick| select(pick))
        .menu_width(menu_width.max(120.0))
        .menu_padding([6.0, MENU_SIDE])
        .menu_text_size(style::SMALL)
        .menu_gap(6.0)
        .text_size(style::TINY)
        .padding(Padding {
            top: 3.0,
            bottom: 3.0,
            left: 6.0,
            right: CARET_ROOM,
        })
        .width(width)
        .handle(pick_list::Handle::None)
        .style(style::control)
        .menu_style(style::control_menu);
    // a thin chevron in place of iced's filled triangle, quieter than the label; an image over
    // the list's right edge, so a press still reaches the list
    let caret = container(icons::glyph("chevron-down", 9.0, alpha(P.fg, 0.5)))
        .padding(Padding {
            right: 6.0,
            ..Padding::ZERO
        })
        .align_right(width)
        .center_y(Fill);
    let list = stack![list, caret];
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

/// GTK's `Conversation::mark_area`: a 12 px drawing with a 1.3 px round stroke.
/// Keep these paths independent of the text font, as in the shelf.
fn status_mark<'a>(status: &str) -> Element<'a, Message> {
    let (shape, color) = match status.to_ascii_lowercase().replace('_', "").as_str() {
        "completed" => (r#"<path d="M2.5 6.5 5 9 9.5 3.5"/>"#, alpha(P.fg, 0.375)),
        "failed" => (r#"<path d="M3.5 3.5 8.5 8.5M8.5 3.5 3.5 8.5"/>"#, P.error),
        "inprogress" => (r#"<circle cx="6" cy="6" r="3"/>"#, P.accent),
        _ => (
            r#"<circle cx="6" cy="6" r="1.2" fill="black" stroke="none"/>"#,
            alpha(P.fg, 0.3),
        ),
    };
    let handle = svg::Handle::from_memory(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="black" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round">{shape}</svg>"#
    ).into_bytes());
    container(svg(handle).width(12).height(12).style(style::icon(color)))
        .padding([0, 2])
        .into()
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
/// The spinner's size and stroke, and its arc: one turn a second (`shelf/spinner.rs`).
const SPIN_SIZE: f32 = 15.0;
const SPIN_LINE: f32 = 2.0;
const SPIN_ARC: f32 = 1.3 * std::f32::consts::PI / 2.0;
/// The gap between the clock and the spinner, and how long the clock takes to slide out.
const CLOCK_GAP: f32 = 8.0;
const CLOCK_SLIDE: Duration = Duration::from_millis(260);

/// The timer: the time worked — going on from `base` while the agent works (`ticking`), else
/// `ms` — `width` wide, in view since `shown`.
#[derive(Clone, Copy)]
struct Clock {
    base: Instant,
    ticking: bool,
    ms: u64,
    shown: Instant,
    width: f32,
}

/// What the activity shows at its right: the spinner while the agent is busy, else the status dot.
#[derive(Clone, Copy)]
enum Indicator {
    Spinner(Color),
    Dot(Color),
}

/// The activity: its indicator, with the timer beside it when shown.
fn activity_canvas<'a>(indicator: Indicator, clock: Option<Clock>) -> Element<'a, Message> {
    let width = clock.map_or(SPIN_SIZE, |c| c.width + CLOCK_GAP + SPIN_SIZE);
    canvas(Busy { indicator, clock })
        .width(width)
        .height(22)
        .into()
}

/// Drawn every frame from the clock's time, so it turns smoothly without the window building a
/// new view for each frame (the shelf's spinner follows its frame clock the same way).
struct Busy {
    indicator: Indicator,
    clock: Option<Clock>,
}

impl Busy {
    /// Anything moving: the spinner, a ticking timer, or the timer still sliding out.
    fn moving(&self) -> bool {
        matches!(self.indicator, Indicator::Spinner(_))
            || self
                .clock
                .is_some_and(|c| c.ticking || c.shown.elapsed() < CLOCK_SLIDE)
    }
}

impl canvas::Program<Message> for Busy {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &iced::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        // only while something moves: an idle dot asks for no frames
        (self.moving()
            && matches!(
                event,
                iced::Event::Window(iced::window::Event::RedrawRequested(_))
            ))
        .then(canvas::Action::request_redraw)
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let now = Instant::now();
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let center = iced::Point::new(bounds.width - SPIN_SIZE / 2.0, bounds.height / 2.0);
        let radius = SPIN_SIZE / 2.0 - SPIN_LINE;
        let stroke = |color: Color| {
            canvas::Stroke::default()
                .with_width(SPIN_LINE)
                .with_color(color)
                .with_line_cap(canvas::LineCap::Round)
        };
        match self.indicator {
            Indicator::Spinner(color) => {
                frame.stroke(
                    &canvas::Path::circle(center, radius),
                    stroke(alpha(color, 0.18)),
                );
                let turn = (spin_epoch().elapsed().as_micros() % 1_000_000) as f32 / 1_000_000.0;
                let start = turn * std::f32::consts::TAU;
                let arc = canvas::Path::new(|path| {
                    path.arc(canvas::path::Arc {
                        center,
                        radius,
                        start_angle: Radians(start),
                        end_angle: Radians(start + SPIN_ARC),
                    });
                });
                frame.stroke(&arc, stroke(alpha(color, 0.95)));
            }
            // the shelf's status square: 8 px, round
            Indicator::Dot(color) => frame.fill(&canvas::Path::circle(center, 4.0), color),
        }
        if let Some(shown_clock) = self.clock {
            // out from under the spinner: from its middle to its place, easing out, fading in
            let t = (now
                .saturating_duration_since(shown_clock.shown)
                .as_secs_f32()
                / CLOCK_SLIDE.as_secs_f32())
            .clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let end = shown_clock.width;
            let from = bounds.width - SPIN_SIZE / 2.0;
            let x = from + (end - from) * eased;
            let ms = if shown_clock.ticking {
                now.saturating_duration_since(shown_clock.base).as_millis() as u64
            } else {
                shown_clock.ms
            };
            let visible = Rectangle::new(
                iced::Point::ORIGIN,
                iced::Size::new(bounds.width - SPIN_SIZE - 1.0, bounds.height),
            );
            frame.with_clip(visible, |frame| {
                frame.fill_text(canvas::Text {
                    content: clock(ms),
                    position: iced::Point::new(x, bounds.height / 2.0),
                    color: alpha(P.fg_dim, eased),
                    size: style::TEXT.into(),
                    font: MONO_FONT,
                    align_x: text::Alignment::Right,
                    align_y: iced::alignment::Vertical::Center,
                    ..canvas::Text::default()
                });
            });
        }
        vec![frame.into_geometry()]
    }
}

/// One clock for every spinner, so two in view turn together.
fn spin_epoch() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}
