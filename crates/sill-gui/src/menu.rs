//! The `+` menu (reference `09`): one row per agent that can start here, with a
//! ghost button at its right; then *Reopen* — a filter, and the sessions not open as tabs, newest
//! first, the first few and "N more…". A row reopens its session; *New here* starts a new one
//! that follows an ended session; the bin deletes, on a second press.
use iced::widget::{
    Column, button, column, container, mouse_area, row, rule, scrollable, space, svg, text,
    text_input, tooltip,
};
use iced::{Alignment, Element, Fill, Padding};
use sill_client::sessions;
use sill_proto::agent_admin::Definition;

use crate::layout::Mode as View;
use crate::style::{self, P, alpha};
use crate::{App, Message, icons, keys};

/// Rows shown before "N more…".
const SHOWN: usize = 5;

#[derive(Default)]
pub struct Menu {
    /// The agents a session can start with; `None` while they load.
    pub agents: Option<Result<Vec<Definition>, String>>,
    /// Settings → Remember pins.
    pub remember_pins: bool,
    pub filter: String,
    pub expanded: bool,
    /// The session whose delete was pressed once.
    pub armed: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Toggle,
    /// The pointer came over the +: the menu opens without a click (and stays if already open).
    Hover,
    /// The pointer left the menu: it closes.
    Leave,
    Agents(Result<Vec<Definition>, String>),
    RememberPins(bool),
    Filter(String),
    More,
    New { agent: String, ghost: bool },
    Reopen(String),
    NewHere(String),
    Delete(String),
    View(View),
}

fn event(event: Event) -> Message {
    Message::Menu(event)
}

impl App {
    /// The menu over the window: a press outside it closes it.
    pub(crate) fn session_menu<'a>(&'a self, menu: &'a Menu) -> Element<'a, Message> {
        let mut panel = Column::new().spacing(2).padding(6).width(330);
        match &menu.agents {
            None => panel = panel.push(caption("Reading the agents…")),
            Some(Err(error)) => {
                panel = panel
                    .push(container(text(error).size(style::TINY).color(P.error)).padding([4, 8]))
            }
            Some(Ok(agents)) => {
                for agent in agents {
                    let start = keys::press(
                        button(
                            row![
                                svg(icons::for_agent(&agent.id))
                                    .width(15)
                                    .height(15)
                                    .style(style::icon(P.fg)),
                                text(&agent.name).size(style::TEXT),
                            ]
                            .spacing(10)
                            .align_y(Alignment::Center),
                        )
                        .padding([6, 8])
                        .width(Fill)
                        .style(style::row_button),
                        event(Event::New {
                            agent: agent.id.clone(),
                            ghost: false,
                        }),
                    );
                    let ghost = tooltip(
                        keys::press(
                            button(
                                svg(icons::ghost())
                                    .width(15)
                                    .height(15)
                                    .style(style::icon(alpha(P.fg, 0.6))),
                            )
                            .padding([6, 8])
                            .style(style::quiet),
                            event(Event::New {
                                agent: agent.id.clone(),
                                ghost: true,
                            }),
                        ),
                        tip(format!(
                            "{} as a ghost — leaves no session behind: no pins, no memory, its writes revert after an hour",
                            agent.name
                        )),
                        tooltip::Position::Left,
                    );
                    panel = panel.push(row![start, ghost].align_y(Alignment::Center));
                }
            }
        }
        let open: Vec<String> = self.tabs.iter().map(|t| t.id.clone()).collect();
        let reopenable = sessions::reopenable(&self.all_sessions, &open);
        if !reopenable.is_empty() {
            panel = panel
                .push(rule::horizontal(1).style(style::rule_style(P.border)))
                .push(caption("Reopen"))
                .push(
                    container(
                        text_input("Filter titles and summaries", &menu.filter)
                            .size(style::SMALL)
                            .padding(6)
                            .on_input(|value| event(Event::Filter(value))),
                    )
                    .padding([2, 4]),
                );
            let filtering = !menu.filter.trim().is_empty();
            let found: Vec<_> = reopenable
                .into_iter()
                .filter(|s| !filtering || sessions::matches(s, &menu.filter))
                .collect();
            let shown = if filtering || menu.expanded {
                found.len()
            } else {
                found.len().min(SHOWN)
            };
            let mut rows = Column::new().spacing(1);
            for session in &found[..shown] {
                rows = rows.push(self.reopen_row(menu, session));
            }
            panel = panel.push(
                scrollable(rows)
                    .direction(scrollable::Direction::Vertical(style::bar()))
                    .height(iced::Shrink)
                    .style(style::feed_scroll),
            );
            if found.is_empty() {
                panel = panel.push(caption("Nothing found"));
            } else if shown < found.len() {
                panel = panel.push(keys::press(
                    button(
                        text(format!("{} more…", found.len() - shown))
                            .size(style::SMALL)
                            .color(P.fg_dim),
                    )
                    .padding([4, 8])
                    .style(style::quiet),
                    event(Event::More),
                ));
            }
        }
        let panel = container(column![
            scrollable(panel)
                .direction(scrollable::Direction::Vertical(style::bar()))
                .height(iced::Shrink)
                .style(style::feed_scroll),
            rule::horizontal(1).style(style::rule_style(P.border)),
            self.view_choice(),
        ])
        .width(330)
        .max_height(640)
        .style(|_| iced::widget::container::Style {
            background: Some(P.surface.into()),
            border: iced::Border {
                color: P.border,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..iced::widget::container::Style::default()
        });
        let placed = container(panel)
            .padding(Padding {
                top: 38.0,
                right: 12.0,
                bottom: 0.0,
                left: 0.0,
            })
            .align_right(Fill);
        // a press anywhere else closes the menu, and so does the pointer leaving it (it opened as
        // the pointer came over the +, which the menu's top padding covers)
        iced::widget::stack![
            // Leave the top bar uncovered so presses on + reach its open-only handler.
            column![
                space().height(37),
                mouse_area(container(space()).width(Fill).height(Fill))
                    .on_press(event(Event::Leave)),
            ]
            .height(Fill),
            mouse_area(placed).on_exit(event(Event::Leave)),
        ]
        .into()
    }

    /// The menu's foot: which view the window shows — the conversation with files, alone, or
    /// across the whole window. The view in use is marked.
    fn view_choice(&self) -> Element<'_, Message> {
        let current = self.layout.mode;
        let choice = |view: View, glyph: &'static str, label: &'static str, what: &'static str| {
            let on = view == current;
            let color = if on { P.accent } else { alpha(P.fg, 0.7) };
            let body = icons::glyph(glyph, 17.0, color);
            let underline = rule::horizontal(2).style(style::rule_style(if on {
                P.accent
            } else {
                iced::Color::TRANSPARENT
            }));
            tooltip(
                container(keys::press(
                    button(
                        column![container(body).padding([8, 0]).center_x(Fill), underline]
                            .width(Fill),
                    )
                    .padding(0)
                    .width(Fill)
                    .style(style::quiet),
                    event(Event::View(view)),
                ))
                .id(match view {
                    View::Shelf => "view-shelf",
                    View::Drawer => "view-drawer",
                    View::Full => "view-full",
                }),
                tip(format!("{label}: {what}")),
                tooltip::Position::Bottom,
            )
        };
        container(
            row![
                choice(
                    View::Shelf,
                    "panel",
                    "Shelf",
                    "the conversation alone, narrow: at most a share of the screen (Ctrl+Shift+D)"
                ),
                choice(
                    View::Drawer,
                    "panel-right",
                    "Drawer",
                    "the conversation with the drawer beside it (Ctrl+Shift+D)"
                ),
                choice(
                    View::Full,
                    "window-full",
                    "Full",
                    "the conversation across the whole window; Ctrl+Shift+F goes back (Ctrl+Shift+F)"
                ),
                // not a mode: the window goes, the daemon and its sessions go on
                tooltip(
                    container(keys::press(
                        button(
                            column![
                                container(icons::glyph("x", 17.0, alpha(P.fg, 0.7)))
                                    .padding([8, 0])
                                    .center_x(Fill),
                                rule::horizontal(2)
                                    .style(style::rule_style(iced::Color::TRANSPARENT)),
                            ]
                            .width(Fill),
                        )
                        .padding(0)
                        .width(Fill)
                        .style(style::quiet),
                        Message::Quit,
                    ))
                    .id("close-window"),
                    tip("Close: the window goes; the daemon and the sessions go on. The tray icon or the hotkey will bring it back (Ctrl+Q)".into()),
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(2),
        )
        .padding(6)
        .into()
    }

    fn reopen_row<'a>(
        &'a self,
        menu: &'a Menu,
        session: &'a sill_proto::sessions::Session,
    ) -> Element<'a, Message> {
        let sub = if session.ghost {
            "ghost · still running".to_string()
        } else if session.ended_at.is_some() {
            session
                .ended_at
                .as_deref()
                .and_then(|t| t.parse::<i64>().ok())
                .map_or_else(|| "ended".into(), date)
        } else if session.running {
            "running".into()
        } else {
            "not running".into()
        };
        let sub = match session.name.as_deref().filter(|name| !name.is_empty()) {
            Some(name) => format!("{name} · {sub}"),
            None => sub,
        };
        let main = keys::press(
            button(
                row![
                    svg(icons::for_agent(&session.agent))
                        .width(15)
                        .height(15)
                        .style(style::icon(P.fg)),
                    column![
                        container(
                            text(crate::ui::one_line(sessions::display_title(session), 34))
                                .size(style::TEXT)
                                .wrapping(text::Wrapping::None),
                        )
                        .width(Fill)
                        .clip(true),
                        text(sub).size(style::TINY).color(P.fg_dim),
                    ]
                    .spacing(1)
                    .width(Fill),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([4, 8])
            .width(Fill)
            .style(style::row_button),
            event(Event::Reopen(session.session_id.clone())),
        );
        let main: Element<'a, Message> = if session.summary.is_empty() {
            main
        } else {
            tooltip(
                main,
                tip(crate::ui::one_line(&session.summary, 400)),
                tooltip::Position::Left,
            )
            .into()
        };
        let mut line = row![main].align_y(Alignment::Center);
        if session.ended_at.is_some() && !session.summary.is_empty() {
            line = line.push(tooltip(
                keys::press(
                    button(
                        svg(icons::plus())
                            .width(13)
                            .height(13)
                            .style(style::icon(alpha(P.fg, 0.6))),
                    )
                    .padding([6, 6])
                    .style(style::quiet),
                    event(Event::NewHere(session.session_id.clone())),
                ),
                tip("New here: a new session with the same agent, policy and pins, told this one's summary first".into()),
                tooltip::Position::Left,
            ));
        }
        let armed = menu.armed.as_deref() == Some(session.session_id.as_str());
        line = line.push(tooltip(
            keys::press(
                button(if armed {
                    Element::from(text("Delete?").size(style::SMALL).color(P.error))
                } else {
                    icons::glyph("trash", 13.0, alpha(P.fg, 0.5)).into()
                })
                .padding([6, 6])
                .style(style::quiet),
                event(Event::Delete(session.session_id.clone())),
            ),
            tip(if armed {
                "Press again to delete the session and its recorded changes".into()
            } else {
                "Delete the session and its recorded changes (press twice)".into()
            }),
            tooltip::Position::Left,
        ));
        line.into()
    }
}

fn caption<'a>(value: &'a str) -> Element<'a, Message> {
    container(text(value).size(style::TINY).color(P.fg_dim))
        .padding([4, 8])
        .into()
}

fn tip<'a>(value: String) -> Element<'a, Message> {
    container(text(value).size(style::TINY))
        .padding([4, 8])
        .max_width(320)
        .style(style::fill(P.code_bg))
        .into()
}

/// A Unix time as sessions are dated: year-month-day, in UTC.
pub(crate) fn date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    // civil from days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_the_days_they_are() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_793_806_135), "2026-11-04");
    }
}
