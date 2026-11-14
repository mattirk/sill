//! Agent Markdown as the shelf shows it: selectable prose (iced_selection's helpers for headings,
//! paragraphs and lists) and fenced code on the code background with a copy button that shows
//! while the pointer is over the block (`.sill-code`, `.sill-copy`). Everything else — quotes,
//! tables, rules, images — keeps iced's defaults for now.
use iced::widget::markdown::{Bullet, HeadingLevel, Settings, Text, Uri, Viewer};
use iced::widget::{button, column, container, hover, right, scrollable, text};
use iced::{Element, Fill, Font, Padding};

use crate::Message;
use crate::style::{self, P};

pub struct SillViewer;

impl<'a> Viewer<'a, Message> for SillViewer {
    fn on_link_click(url: Uri) -> Message {
        Message::Link(url)
    }

    fn heading(
        &self,
        settings: Settings,
        level: &'a HeadingLevel,
        text: &'a Text,
        index: usize,
    ) -> Element<'a, Message> {
        iced_selection::markdown::heading(settings, level, text, index, Message::Link)
    }

    /// As iced_selection's paragraph, with inline code a size smaller: the code face is wider
    /// than the prose face, and the shelf sets code one step down (`.sill-code`: 92 %).
    fn paragraph(&self, settings: Settings, text: &Text) -> Element<'a, Message> {
        let code = Font::with_name(crate::fonts::MONO_FAMILY);
        let spans: Vec<_> = text
            .spans(settings.style)
            .iter()
            .cloned()
            .map(|span| {
                if span.font == Some(code) {
                    span.size(settings.code_size)
                } else {
                    span
                }
            })
            .collect();
        iced_selection::rich_text(spans)
            .size(settings.text_size)
            .on_link_click(Message::Link)
            .into()
    }

    fn unordered_list(&self, settings: Settings, bullets: &'a [Bullet]) -> Element<'a, Message> {
        iced_selection::markdown::unordered_list(self, settings, bullets)
    }

    fn ordered_list(
        &self,
        settings: Settings,
        start: u64,
        bullets: &'a [Bullet],
    ) -> Element<'a, Message> {
        iced_selection::markdown::ordered_list(self, settings, start, bullets)
    }

    fn code_block(
        &self,
        settings: Settings,
        _language: Option<&'a str>,
        code: &'a str,
        lines: &'a [Text],
    ) -> Element<'a, Message> {
        let body = container(
            scrollable(
                container(column(lines.iter().map(|line| {
                    iced_selection::rich_text(line.spans(settings.style))
                        .on_link_click(Message::Link)
                        .font(Font::with_name(crate::fonts::MONO_FAMILY))
                        .size(settings.code_size)
                        .into()
                })))
                .padding(Padding::from([8.0, 12.0])),
            )
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::default().width(4).scroller_width(4),
            ))
            .style(style::feed_scroll),
        )
        .width(Fill)
        .style(style::fill(P.code_bg));
        let copy = right(
            button(text("Copy").size(style::TINY))
                .padding([2, 8])
                .style(style::quiet)
                .on_press_with(move || Message::Copy(code.to_string())),
        )
        .padding([4, 6]);
        container(hover(body, copy))
            .padding(Padding::from([2.0, 8.0]))
            .into()
    }
}
