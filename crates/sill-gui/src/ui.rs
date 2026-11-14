//! The window (`docs/reference-ui` 01–02 and 19–22): a top bar with the
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
/// The window's resize edges, over its outer pixels: wide enough (the person, 2026-11-07).
const EDGE: f32 = 5.0;

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let content = responsive(move |size| {
            // The sill pages take the conversation's place.
            if self.agents_page.shown {
                return self.agents_view();
            }
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
            let body = if let Some(editor) = &view.annotation {
                column![crate::annotation::view(editor), self.composer(view)]
                    .height(Fill)
                    .into()
            } else if let Some(details) = &self.details {
                self.details_view(details)
            } else {
                match self.file_view(view) {
                    Some(reader) => reader,
                    // the test desktop in the conversation's place, the composer staying
                    None if view.desktop.shown => {
                        let session = self.all_sessions.iter().find(|s| s.session_id == view.id);
                        column![crate::desktop_view::pane(view, session), self.composer(view)]
                            .height(Fill)
                            .into()
                    }
                    None => column![self.feed(view), self.composer(view)]
                        .height(Fill)
                        .into(),
                }
            };
            // a file open takes the chips' row too
            let chips: Element<'_, Message> =
                if self.details.is_some() || view.files.reader.is_some() {
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
        let edge = |direction, horizontal| {
            mouse_area(
                container(space())
                    .width(if horizontal {
                        Fill
                    } else {
                        iced::Length::Fixed(EDGE)
                    })
                    .height(if horizontal {
                        iced::Length::Fixed(EDGE)
                    } else {
                        Fill
                    }),
            )
            .on_press(Message::ResizeWindow(direction))
            .interaction(if horizontal {
                iced::mouse::Interaction::ResizingVertically
            } else {
                iced::mouse::Interaction::ResizingHorizontally
            })
        };
        // where two edges meet: both ways at once (the same 5 px, no larger target)
        let corner = |direction, falling| {
            mouse_area(container(space()).width(EDGE).height(EDGE))
                .on_press(Message::ResizeWindow(direction))
                .interaction(if falling {
                    iced::mouse::Interaction::ResizingDiagonallyDown
                } else {
                    iced::mouse::Interaction::ResizingDiagonallyUp
                })
        };
        // the window-level Host control line: every session with active control, whichever shows
        let mut window = column![bar];
        if let Some(band) = crate::host_control::band(self) {
            window = window.push(band);
        }
        let window = window.push(content);
        // Keep the conversation at the same tree position when the menu opens/closes.
        // Wrapping it in a stack only while open discards its scroll and selection state.
        let mut layers = stack![window];
        if let Some(menu) = &self.menu {
            layers = layers.push(self.session_menu(menu));
        }
        // Resize targets overlay the outer pixels; they must not inset the content. Shelf mode is
        // locked to its edge at the full height: only its right edge, corners included, changes
        // its width. A free window has every edge, and its corners resize both ways.
        use iced::window::Direction;
        layers = layers.push(container(edge(Direction::East, false)).align_right(Fill));
        if self.layout.mode != layout::Mode::Shelf {
            layers = layers
                .push(container(edge(Direction::West, false)).align_left(Fill))
                .push(container(edge(Direction::North, true)).align_top(Fill))
                .push(container(edge(Direction::South, true)).align_bottom(Fill))
                .push(
                    container(corner(Direction::NorthWest, true))
                        .align_left(Fill)
                        .align_top(Fill),
                )
                .push(
                    container(corner(Direction::NorthEast, false))
                        .align_right(Fill)
                        .align_top(Fill),
                )
                .push(
                    container(corner(Direction::SouthWest, false))
                        .align_left(Fill)
                        .align_bottom(Fill),
                )
                .push(
                    container(corner(Direction::SouthEast, true))
                        .align_right(Fill)
                        .align_bottom(Fill),
                );
        }
        let annotating = !self.backend_shown()
            && self.current().is_some_and(|view| view.annotation.is_some());
        keys::annotation_shortcuts(
            keys::configured_shortcuts(layers, &self.settings.shortcuts),
            annotating,
        )
    }

    /// The drawer's edge: flush with the conversation, with a wider drag target inside the drawer.
    fn drawer_grip(&self) -> Element<'_, Message> {
        let line = if self.layout.dragging {
            P.accent
        } else {
            P.border
        };
        mouse_area(
            container(rule::vertical(1).style(style::rule_style(line)))
                .align_left(layout::HANDLE)
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
            "No open session. Start one with +; it shows here as a tab.".into()
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
        let shown = self.backend_shown();
        let drawer_open = self.workspace.shown && !shown && self.current().is_some();
        // room on its right
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
                    "Sill: Changes, Agents and Settings"
                })
                .size(style::TINY),
            )
            .padding([3, 8])
            .style(style::fill(P.surface)),
            tooltip::Position::Bottom,
        );
        let mut tabs = row![].spacing(2).height(Fill);
        if shown {
            // the pages' own tabs in place of the sessions'
            for (page, glyph, title) in [
                (crate::Page::Changes, "changes", "Changes"),
                (crate::Page::Agents, "agents", "Agents"),
                (crate::Page::Settings, "settings", "Settings"),
            ] {
                tabs = tabs.push(self.page_tab(page, glyph, title));
            }
        } else {
            for tab in &self.tabs {
                tabs = tabs.push(self.tab(tab));
            }
        }
        // the window has no title bar: a press here that no tab or button takes moves it (not in
        // Shelf mode, which is locked to its edge)
        let bar = mouse_area(
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
                            // Hover opens the menu; pressing + keeps it open, including by keyboard.
                            mouse_area(keys::press(
                                button(
                                    svg(icons::plus())
                                        .width(15)
                                        .height(15)
                                        .style(style::icon(alpha(P.fg, 0.75))),
                                )
                                .padding(4)
                                .style(style::quiet),
                                Message::Menu(crate::menu::Event::Hover),
                            ))
                            .on_enter(Message::Menu(crate::menu::Event::Hover)),
                            // the activity over the drawer when it is beside the conversation, so the +
                            // ends the conversation's side
                            match self.layout.drawer(drawer_open, width) {
                                // reaching back across the bar's spacing and the +'s padding, so the +
                                // sits about 10 px left of the divider
                                Drawer::Beside(drawer) => container(activity)
                                    .width(drawer + layout::HANDLE - 14.0)
                                    .align_right(drawer + layout::HANDLE - 14.0),
                                _ => container(activity),
                            },
                        ]
                        // A little more room after + shifts it left without shrinking its hit area.
                        .spacing(9)
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
        );
        if self.layout.mode == layout::Mode::Shelf {
            return bar.into();
        }
        bar.on_press(Message::DragWindow).into()
    }

    /// A page's tab: its glyph and title, underlined while it shows.
    fn page_tab<'a>(
        &'a self,
        page: crate::Page,
        glyph: &'static str,
        title: &'static str,
    ) -> Element<'a, Message> {
        let active = match page {
            crate::Page::Agents => self.agents_page.shown,
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
            text(self.session_name(&tab.id, &tab.title))
                .size(style::TEXT)
                .color(color),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(mark) = attention.as_ref().and_then(mark) {
            label = label.push(mark);
        }
        let agent = keys::press(
            button(icons::tab_agent(
                &tab.agent,
                self.agent_icons.get(&tab.agent).map(Vec::as_slice),
                if working { P.accent } else { color },
                working,
            ))
            .padding([4, 4])
            .style(style::quiet),
            Message::Details(crate::details::Event::Open(tab.id.clone())),
        );
        let label: Element<'a, Message> = if let Some(editor) =
            self.name_editor.as_ref().filter(|e| e.id == tab.id)
        {
            let input = iced::widget::text_input("Session name", &editor.text)
                .id("session-name")
                .on_input_maybe(
                    (!editor.pending).then_some(|s| Message::Name(crate::names::Event::Input(s))),
                )
                .on_submit_maybe(
                    (!editor.pending).then_some(Message::Name(crate::names::Event::Save)),
                )
                .size(style::TEXT)
                .width((editor.text.chars().count() as f32 * 8.0 + 24.0).clamp(120.0, 320.0));
            let input: Element<'a, Message> = input.into();
            if let Some(error) = &editor.error {
                tooltip(
                    row![input, text("!").color(P.error)].spacing(4),
                    text(error).size(style::SMALL),
                    tooltip::Position::Bottom,
                )
                .into()
            } else {
                input
            }
        } else {
            keys::press(
                button(
                    mouse_area(label)
                        .on_press(Message::Choose(tab.id.clone()))
                        .on_double_click(Message::Name(crate::names::Event::Begin(tab.id.clone()))),
                )
                .padding([4, 12])
                .style(style::quiet),
                Message::Choose(tab.id.clone()),
            )
        };
        let label: Element<'a, Message> = row![agent, label].align_y(Alignment::Center).into();
        // Show the close button on every tab, including inactive tabs.
        // Closing ends the session (a ghost asks once: it deletes it).
        // The 12px glyph plus its 6px padding on each side.
        const CLOSE_WIDTH: f32 = 24.0;
        let label: Element<'a, Message> = if !self.demo {
            let armed = self.armed_close.as_deref() == Some(tab.id.as_str());
            row![
                label,
                tooltip(
                    container(keys::press(
                        button(icons::glyph(
                            "x",
                            12.0,
                            if armed { P.error } else { alpha(P.fg, 0.55) },
                        ))
                        .width(CLOSE_WIDTH)
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
            row![label, space().width(if self.demo { 0.0 } else { CLOSE_WIDTH })]
                .align_y(Alignment::Center)
                .into()
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

    /// Agent activity while connected; otherwise the daemon status, in the existing slot.
    fn activity(&self) -> Element<'_, Message> {
        let Some(view) = self.current() else {
            return self.daemon_indicator(activity_canvas(self.daemon_dot(), None));
        };
        let attention = view.attention();
        let trouble = view
            .status
            .problem()
            .map(str::to_owned)
            .or_else(|| self.list_error.clone());
        let busy_now = trouble.is_none()
            && matches!(attention.activity, Activity::Working | Activity::Waiting);
        let indicator = if busy_now && (!self.daemon_trouble() || self.demo) {
            Indicator::Spinner(P.accent)
        } else {
            self.daemon_dot()
        };
        // the timer: while the agent works, and whenever the pointer is over the
        // activity — the session's worked time, still while it does not work; it slides out from
        // under the indicator when it appears
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
            Clock {
                ticking: attention.working_ms.is_some(),
                ms,
                shown,
                width: clock_width(ms),
            }
        });
        if clock.is_none() {
            view.clock_shown.set(None);
        }
        let shown = activity_canvas(indicator, clock);
        self.daemon_indicator(shown)
    }

    fn daemon_dot(&self) -> Indicator {
        if self.demo {
            Indicator::Dot(alpha(P.fg, 0.35))
        } else if self.daemon.stopped && !self.daemon.restarting {
            Indicator::Dot(alpha(P.error, 0.7))
        } else {
            Indicator::Pulse(if self.daemon_trouble() {
                P.warning
            } else {
                P.success
            })
        }
    }

    fn daemon_indicator<'a>(&'a self, shown: Element<'a, Message>) -> Element<'a, Message> {
        let shown = tooltip(
            shown,
            container(
                text(if self.demo {
                    "Demo — no daemon".into()
                } else {
                    self.daemon_label()
                })
                .size(style::TINY),
            )
            .padding([3, 8])
            .style(style::fill(P.surface)),
            tooltip::Position::Bottom,
        );
        let area = mouse_area(shown).on_enter(Message::ActivityHover(true));
        if !self.demo && self.daemon_trouble() {
            area.on_press(Message::Daemon(crate::daemon_ui::Event::Open))
                .into()
        } else {
            area.into()
        }
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
                            // Form edits do not change height: text inputs are single-line
                            // and choice marks keep their size. Invalidating this measurement
                            // on each keystroke makes virtualization temporarily count the row
                            // as zero height, which can remove it and lose input focus.
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
        // the agent asks for a login: its card, after everything else
        if let Some(card) = self.login_card(view) {
            feed = feed.push(rows::key("login"), card);
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
        let mut body = Column::new();
        let empty_user = item.kind == "user"
            && view
                .rendered
                .get(&item.item_id)
                .is_none_or(|r| r.source.trim().is_empty());
        if !empty_user {
            body = body.push(self.item_body(view, item));
        }
        let mut photos = row![].spacing(6);
        let mut count = 0;
        let keys = crate::media::item_keys(item);
        for key in &keys {
            if item.kind == "user" {
                photos = photos.push(crate::media::thumbnail(&view.media, key, 72.0, 52.0));
                count += 1;
            } else {
                body = body.push(crate::media::picture(&view.media, &view.id, key));
            }
        }
        if let Some(submission) = item
            .submission_id
            .as_ref()
            .and_then(|id| view.conversation.submissions.get(id))
        {
            for attachment in &submission.attachments {
                if crate::media::is_image(attachment) {
                    let key = crate::media::key(attachment);
                    if !keys.contains(&key) {
                        photos =
                            photos.push(crate::media::thumbnail(&view.media, &key, 72.0, 52.0));
                        count += 1;
                    }
                } else {
                    body = body.push(
                        text(crate::media::name(attachment))
                            .size(style::SMALL)
                            .color(P.fg_dim),
                    );
                }
            }
        }
        if count > 0 {
            body = body.push(
                container(
                    scrollable(photos)
                        .direction(scrollable::Direction::Horizontal(style::bar()))
                        .style(style::feed_scroll),
                )
                .padding([2, 14]),
            );
        }
        body.spacing(6).into()
    }

    fn item_body<'a>(&'a self, view: &'a SessionView, item: &'a Item) -> Element<'a, Message> {
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
            // the agent's reasoning: Markdown (Codex's summaries are bold
            // titles after blank lines), smaller, dimmed and in italics
            "thought" => match view.rendered.get(&item.item_id) {
                Some(rendered) => container(markdown::view_with(
                    rendered.markdown.items(),
                    thought_settings(),
                    &viewer::SillViewer {
                        media: Some((&view.id, &view.media)),
                    },
                ))
                .padding([0, 8])
                .width(Fill)
                .style(|_| container::Style {
                    text_color: Some(alpha(P.fg, 0.55)),
                    ..container::Style::default()
                })
                .into(),
                None => space().into(),
            },
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
                if is_long_notice(source) {
                    folded_note(&item.item_id, source, color, view.open_tools.contains(&item.item_id))
                } else {
                    note(source, color)
                }
            }
            "plan" => plan(item),
            // agent text and anything newer: Markdown as the agent wrote it
            _ => match view.rendered.get(&item.item_id) {
                Some(rendered) => container(markdown::view_with(
                    rendered.markdown.items(),
                    markdown_settings(),
                    &viewer::SillViewer {
                        media: Some((&view.id, &view.media)),
                    },
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

    /// The composer's bar, `width` wide: the shell's button, the agent's controls (scrolling
    /// sideways when they do not fit: Interface scale 150 % in Shelf mode) and send or stop,
    /// which stay in reach.
    fn composer_bar<'a>(&'a self, view: &'a SessionView, width: f32) -> Element<'a, Message> {
        /// What the shell's and the send/stop buttons take with their spacing, and a margin.
        const BUTTONS_ROOM: f32 = 142.0;
        let has_session = !view.conversation.session_id.is_empty() || self.demo;
        let sendable = view.annotation.is_none()
            && !view.attachments_busy
            && (!view.draft.text().trim().is_empty() || !view.attachments.is_empty());
        let action: Element<'_, Message> = if view.conversation.busy() {
            stop_button()
        } else {
            keys::press_maybe(
                button(svg(icons::send()).width(12).height(12).style(
                    move |theme, status| {
                        // dark on the accent only when the button lights up: there is a prompt
                        if sendable {
                            style::send_arrow(theme, status)
                        } else {
                            style::send_arrow(theme, svg::Status::Idle)
                        }
                    },
                ))
                .padding(3)
                .style(style::send),
                sendable.then_some(Message::Send),
            )
        };
        let shell = bar_button(icons::glyph(
            "terminal",
            BAR_ICON,
            if !has_session {
                alpha(P.fg_dim, 0.4)
            } else if view.terminal_shown && view.terminal_host {
                P.warning
            } else if view.terminal_shown {
                P.accent
            } else {
                P.fg_dim
            },
        ))
        .style(style::quiet);
        let shell = container(keys::press_maybe(
            shell,
            (has_session && view.annotation.is_none()).then_some(Message::ToggleTerminal),
        ))
        .id("terminal-button");
        let shell = mouse_area(shell);
        let shell = if has_session && view.annotation.is_none() {
            shell.on_right_press(Message::ToggleHostTerminal)
        } else {
            shell
        };
        let shell = bar_tip(shell, "Left-click: jail terminal · Right-click: host terminal");
        // the agent's controls sideways-scrolling between the shell's button and send/stop, which
        // stay in reach however narrow the window (Interface scale 150 % in Shelf mode)
        let mut agent_controls = row![].spacing(4).align_y(Alignment::Center);
        for control in controls(view) {
            agent_controls = agent_controls.push(control);
        }
        if let Some(processes) = crate::processes::indicator(view) {
            agent_controls = agent_controls.push(processes);
        }
        if let Some(usage) = usage(view) {
            agent_controls = agent_controls
                .push(container(text(usage).size(CONTROL_TEXT).color(P.fg_dim)).padding([0, 6]));
        }
        let agent_controls = scrollable(agent_controls)
            .direction(scrollable::Direction::Horizontal(style::bar()))
            .anchor_right()
            .style(style::feed_scroll)
            .width(Shrink);
        // as wide as its controls, up to the room the two buttons leave: past that it scrolls
        let agent_controls = container(agent_controls).max_width((width - BUTTONS_ROOM).max(0.0));
        let can_attach = has_session && !view.attachments_busy && view.annotation.is_none();
        let attach = bar_tip(
            keys::press_maybe(
                bar_button(icons::glyph(
                    "plus",
                    BAR_ICON,
                    if can_attach {
                        P.fg_dim
                    } else {
                        alpha(P.fg_dim, 0.4)
                    },
                ))
                .style(style::quiet),
                can_attach.then_some(Message::Media(crate::media::Event::Choose)),
            ),
            "Attach files or images",
        );
        let session = self
            .all_sessions
            .iter()
            .find(|s| s.session_id == view.id && has_session);
        let permissions = crate::permissions::indicator(view, session);
        let mut left = row![attach, shell, permissions].spacing(4).align_y(Alignment::Center);
        if let Some(desktop) = crate::desktop_view::indicator(view, session) {
            left = left.push(desktop);
        }
        row![left, space().width(Fill), agent_controls, action]
            .spacing(4)
            .align_y(Alignment::Center)
            .into()
    }

    fn composer<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let session = self.all_sessions.iter().find(|s| s.session_id == view.id);
        let mut composer = Column::new();
        if let Some(caption) = session.and_then(crate::host_control::caption) {
            composer = composer.push(caption);
        }
        if let Some(caption) = crate::permissions::caption(view, session) {
            composer = composer.push(caption);
        }
        // the panel lays its two columns out by the width it actually gets
        let mut composer = composer
            .push(
                responsive(move |size| crate::permissions::panel(view, session, size.width < crate::permissions::NARROW))
                    .height(Shrink),
            )
            .push(crate::processes::panel(view))
            .push(self.network_prompt(view));
        if !view.attachments.is_empty() {
            let mut strip = row![].spacing(6).align_y(Alignment::Center);
            for (index, attachment) in view.attachments.iter().enumerate() {
                let preview: Element<'_, Message> = if crate::media::is_image(attachment) {
                    crate::media::thumbnail(&view.media, &crate::media::key(attachment), 44.0, 34.0)
                } else {
                    container(
                        text(one_line(crate::media::name(attachment), 24))
                            .size(style::TINY)
                            .color(P.fg_dim),
                    )
                    .padding([4, 6])
                    .into()
                };
                let remove = keys::press_maybe(
                    button(text("×").size(style::SMALL).color(P.fg_dim))
                        .padding([2, 4])
                        .style(style::quiet),
                    view.annotation.is_none().then_some(Message::Media(
                        crate::media::Event::Remove(index),
                    )),
                );
                let controls: Element<'_, Message> = if crate::media::is_image(attachment) {
                    let available = view.annotation.is_none()
                        && !view.attachments_busy
                        && matches!(
                            view.media.get(&crate::media::key(attachment)),
                            Some(Some(Ok(_)))
                        );
                    let pen = bar_tip(
                        keys::press_maybe(
                            button(icons::glyph("pen", 11.0, P.fg_dim))
                                .padding([2, 4])
                                .style(style::quiet),
                            available.then(|| Message::Annotation(
                                crate::annotation::Event::Open(attachment.id.clone()),
                            )),
                        ),
                        "Annotate image",
                    );
                    column![remove, pen].align_x(Alignment::Center).into()
                } else {
                    remove
                };
                let chip = row![preview, controls].align_y(Alignment::Center).spacing(1);
                strip = if crate::media::is_image(attachment) {
                    strip.push(chip)
                } else {
                    strip.push(tooltip(
                        chip,
                        text(crate::media::name(attachment)).size(style::TINY),
                        tooltip::Position::Top,
                    ))
                };
            }
            composer = composer.push(
                container(
                    scrollable(strip)
                        .direction(scrollable::Direction::Horizontal(style::bar()))
                        .style(style::feed_scroll),
                )
                .padding([4, 12]),
            );
        }
        if view.attachments_busy {
            composer = composer.push(text("Preparing attachment…").size(style::SMALL));
        }
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
        for (index, submission) in waiting.into_iter().enumerate() {
            has_queued = true;
            queued = queued.push(
                row![
                    text(format!("Queued {}", index + 1))
                        .size(style::TINY)
                        .color(P.fg_dim),
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
                    .padding([3, 0])
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
            .padding([8, 14])
            .size(style::TEXT)
            .height(Shrink)
            .max_height(220)
            .style(style::prompt);
        let editor = if view.annotation.is_none() {
            editor.on_action(Message::Prompt).key_binding(enter_sends)
        } else {
            editor
        };
        // the bar knows its width, so the agent's controls can give way to the buttons
        let bar = responsive(move |size| self.composer_bar(view, size.width)).height(Shrink);
        // the prompt folds away while the terminal is up in its place
        let terminal = view.terminal.as_ref()
            .filter(|_| view.terminal_shown && view.annotation.is_none());
        let input: Element<'_, Message> = match terminal {
            Some(pane) => self.terminal_pane(pane, view.terminal_host),
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
    /// of the host shell's warning-coloured one.
    fn terminal_pane<'a>(
        &'a self,
        pane: &'a crate::terminal::Pane,
        host: bool,
    ) -> Element<'a, Message> {
        use crate::terminal::Status;
        let mut caption = row![
            text(if host {
                "host · outside the jail · writes are not tracked"
            } else {
                "jail · inside the session"
            })
            .size(style::TINY)
            .color(if host { P.warning } else { P.success }),
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
        let mut pane_column = column![
            rule::horizontal(1).style(style::rule_style(P.success)),
            container(caption).padding([2, 12]),
            container(body)
                .height(300)
                .width(Fill)
                .style(style::fill(P.bg)),
        ];
        // what was pasted, under the terminal and over the controls, shown whether or not the
        // program echoes it (a login's code)
        if let Some(pasted) = &pane.pasted {
            pane_column = pane_column.push(
                container(
                    text(format!(
                        "Pasted {} characters: {}",
                        pasted.chars().count(),
                        one_line(pasted, 200)
                    ))
                    .size(style::TINY)
                    .color(alpha(P.fg, 0.75))
                    .wrapping(text::Wrapping::None),
                )
                .width(Fill)
                .clip(true)
                .padding([3, 12]),
            );
        }
        pane_column.into()
    }
}

fn placeholder(view: &SessionView) -> &'static str {
    match view.status {
        Status::NotRunning => "Not running — sending a prompt starts it",
        _ => "",
    }
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
/// The composer bar's quiet buttons are this square, as tall as the agent's controls beside them,
/// so their boxes line up when hovered (send and stop keep their smaller filled square).
pub(crate) const BAR_BUTTON: f32 = 22.0;
/// The line glyphs in the composer bar's quiet buttons.
pub(crate) const BAR_ICON: f32 = 14.0;

/// A composer bar button: `content` centred in a [`BAR_BUTTON`] square.
pub(crate) fn bar_button<'a>(
    content: impl Into<Element<'a, Message>>,
) -> iced::widget::Button<'a, Message> {
    button(center(content))
        .width(BAR_BUTTON)
        .height(BAR_BUTTON)
        .padding(0)
}

/// A composer bar button's tooltip, above it.
pub(crate) fn bar_tip<'a>(
    content: impl Into<Element<'a, Message>>,
    tip: impl text::IntoFragment<'a>,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(tip).size(style::SMALL))
            .padding([4, 8])
            .style(style::fill(P.code_bg)),
        tooltip::Position::Top,
    )
    .into()
}

fn stop_button<'a>() -> Element<'a, Message> {
    container(keys::press(
        button(
            svg(icons::stop())
                .width(12)
                .height(12)
                .style(style::icon(P.error)),
        )
        .padding(3)
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
/// Submit and Decline. Values are encoded by `forms.rs`.
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
                    text(format!(
                        "{}{}",
                        one_line(pending.text(), 100),
                        if pending.submit.attachments.is_empty() {
                            String::new()
                        } else {
                            format!(" · {} attachment(s)", pending.submit.attachments.len())
                        }
                    ))
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
        // the mode set already covers a "mode"-category option (Claude Code sends both)
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

/// An agent control's label: a little smaller than captions, so mode, model and effort stay quiet.
const CONTROL_TEXT: f32 = style::TINY * 0.9;
/// The control's padding above and below its label, keeping it [`BAR_BUTTON`] tall (iced's
/// line height is 1.3 × the text size).
const CONTROL_PAD_Y: f32 = (BAR_BUTTON - CONTROL_TEXT * 1.3) / 2.0;

/// The room a control keeps at its right for its caret.
const CARET_ROOM: f32 = 19.0;

/// One ACP control: as wide as its selected label. iced's pick
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
    // a little under 80 % of the body text; room at the right for the caret
    let width = text_width(label, CONTROL_TEXT) + 6.0 + CARET_ROOM;
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
        .text_size(CONTROL_TEXT)
        .padding(Padding {
            top: CONTROL_PAD_Y,
            bottom: CONTROL_PAD_Y,
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
    text_width_in_font(content, size, Font::with_name(crate::fonts::SANS_FAMILY))
}

fn text_width_in_font(content: &str, size: f32, font: Font) -> f32 {
    use iced::advanced::text::Paragraph as _;
    let paragraph = iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
        content,
        bounds: iced::Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
    });
    paragraph.min_width().ceil()
}

fn history_shortcut(key_press: &text_editor::KeyPress) -> Option<bool> {
    if key_press.modifiers.command()
        && matches!(key_press.status, text_editor::Status::Focused { .. })
        && let Key::Character(c) = key_press.key.as_ref()
        && (c.eq_ignore_ascii_case("z") || c.eq_ignore_ascii_case("y"))
    {
        return Some(key_press.modifiers.shift() || c.eq_ignore_ascii_case("y"));
    }
    None
}

/// Enter sends; Shift+Enter is a new line; everything else is the editor's own.
pub(crate) fn enter_sends(
    key_press: text_editor::KeyPress,
) -> Option<text_editor::Binding<Message>> {
    if let Some(redo) = history_shortcut(&key_press) {
        return Some(text_editor::Binding::Custom(Message::PromptHistory(redo)));
    }
    if matches!(key_press.key.as_ref(), Key::Character(c) if c.eq_ignore_ascii_case("v"))
        && key_press.modifiers.command()
        && matches!(key_press.status, text_editor::Status::Focused { .. })
    {
        return Some(text_editor::Binding::Custom(Message::Media(
            crate::media::Event::Paste,
        )));
    }
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

/// The source reader's keys: Save and window shortcuts first, then the editor's own.
pub(crate) fn reading_keys(
    key_press: text_editor::KeyPress,
) -> Option<text_editor::Binding<Message>> {
    if let Some(redo) = history_shortcut(&key_press) {
        return Some(text_editor::Binding::Custom(Message::Workspace(
            crate::workspace::Action::History(redo),
        )));
    }
    if key_press.modifiers.command()
        && matches!(&key_press.key, iced::keyboard::Key::Character(c) if c.eq_ignore_ascii_case("s"))
    {
        return Some(text_editor::Binding::Custom(Message::Workspace(
            crate::workspace::Action::SaveFile,
        )));
    }
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

/// A thought's Markdown: the conversation's, a size smaller and in italics (`.sill-thought`).
fn thought_settings() -> markdown::Settings {
    let mut style = markdown::Style::from_palette(style::theme().palette());
    style.font = Font {
        style: iced::font::Style::Italic,
        ..Font::with_name(crate::fonts::SANS_FAMILY)
    };
    style.link_color = P.link;
    style.inline_code_color = alpha(P.fg, 0.7);
    style.inline_code_font = MONO_FONT;
    style.code_block_font = MONO_FONT;
    style.inline_code_highlight = Highlight {
        background: P.code_bg.into(),
        border: iced::Border::default(),
    };
    let mut settings = markdown::Settings::with_text_size(style::SMALL, style);
    settings.code_size = (style::MONO * 0.9).into();
    settings.spacing = (style::SMALL * 0.5).into();
    settings
}

/// `content` behind a left rule `width` wide in `color`: how a block's kind is marked.
pub(crate) fn ruled<'a>(
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
/// A notice long enough to bury the conversation (a Host control inspection's process list):
/// folded to its first line until clicked.
pub(crate) fn is_long_notice(message: &str) -> bool {
    message.len() > LONG_NOTICE || message.lines().count() > 3
}
const LONG_NOTICE: usize = 240;

/// A long notice: one pressable line with a chevron, the whole text under it when open (the
/// same open set as tool calls, `Message::ToggleTool`).
fn folded_note<'a>(item_id: &str, message: &'a str, color: Color, open: bool) -> Element<'a, Message> {
    let line = button(
        row![
            text(if open { "▾" } else { "▸" }).size(style::SMALL).color(color).width(14),
            container(
                text(one_line(message, 160))
                    .size(style::SMALL)
                    .color(color)
                    .wrapping(text::Wrapping::None),
            )
            .width(Fill)
            .clip(true),
            text(format!("{} chars", message.len()))
                .size(style::TINY)
                .color(alpha(color, 0.7)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([3, 6])
    .width(Fill)
    .style(style::row_button);
    let line = keys::press(line, Message::ToggleTool(item_id.to_owned()));
    let mut body = Column::new().push(line);
    if open {
        body = body.push(note(message, color));
    }
    ruled(3.0, color, container(body).width(Fill).style(style::fill(alpha(color, 0.08))))
}

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

/// The glyph for an ACP tool kind.
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

/// A tool's status mark: a 12 px drawing with a 1.3 px round stroke.
/// Keep these paths independent of the text font.
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
/// The spinner's size and stroke, and its arc: one turn a second.
const SPIN_SIZE: f32 = 15.0;
const SPIN_LINE: f32 = 2.0;
const SPIN_ARC: f32 = 1.3 * std::f32::consts::PI / 2.0;
/// The gap between the clock and the spinner, and how long the clock takes to slide out.
const CLOCK_GAP: f32 = 8.0;
const CLOCK_SLIDE: Duration = Duration::from_millis(260);

/// Fit the current label in its actual drawing font; grow only when it gains digits.
fn clock_width(ms: u64) -> f32 {
    let label = clock(ms).replace(char::is_numeric, "0");
    text_width_in_font(&label, style::TEXT, MONO_FONT) + 2.0
}

/// Label and width come from the same per-second sample. The canvas animates the
/// reveal/spinner between ticks without advancing text beyond its allocated width.
#[derive(Clone, Copy)]
struct Clock {
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
    Pulse(Color),
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
/// new view for each frame.
struct Busy {
    indicator: Indicator,
    clock: Option<Clock>,
}

impl Busy {
    /// Anything moving: the spinner, a ticking timer, or the timer still sliding out.
    fn moving(&self) -> bool {
        matches!(self.indicator, Indicator::Spinner(_) | Indicator::Pulse(_))
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
        .then(|| {
            if matches!(self.indicator, Indicator::Pulse(_)) && self.clock.is_none() {
                canvas::Action::request_redraw_at(Instant::now() + Duration::from_millis(50))
            } else {
                canvas::Action::request_redraw()
            }
        })
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
            // the status square: 8 px, round
            Indicator::Pulse(color) => {
                let phase = spin_epoch().elapsed().as_secs_f32() * std::f32::consts::TAU / 4.0;
                frame.fill(
                    &canvas::Path::circle(center, 4.0),
                    alpha(color, 0.3 + 0.15 * (1.0 + phase.sin())),
                );
            }
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
            let visible = Rectangle::new(
                iced::Point::ORIGIN,
                iced::Size::new(bounds.width - SPIN_SIZE - 1.0, bounds.height),
            );
            frame.with_clip(visible, |frame| {
                frame.fill_text(canvas::Text {
                    content: clock(shown_clock.ms),
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

#[cfg(test)]
mod timer_tests {
    use super::*;
    #[test]
    fn timer_slot_fits_every_digit_at_minutes_and_hours_boundaries() {
        crate::fonts::install();
        for seconds in [
            0u64, 599, 600, 1199, 1200, 3599, 3600, 35999, 36000, 359999, 360000,
        ] {
            let ms = seconds * 1000;
            for delta in [0, 999] {
                let shown = clock(ms + delta);
                let actual = text_width_in_font(&shown, style::TEXT, MONO_FONT);
                assert!(
                    clock_width(ms) >= actual + 1.0,
                    "{shown} clips in {} px (needs {actual})",
                    clock_width(ms)
                );
            }
        }
        assert!(clock_width(599000) < clock_width(600000));
        assert!(clock_width(3599000) < clock_width(3600000));
        assert_eq!(clock_width(600000), clock_width(3599000));
        assert!(clock_width(35999000) < clock_width(36000000));
        assert!(clock_width(360000000) > clock_width(36000000));
    }
}
