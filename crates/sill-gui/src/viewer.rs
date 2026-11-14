//! Agent Markdown: prose and fenced code on the code background with a copy
//! button that shows while the pointer is over the block (`.sill-code`, `.sill-copy`). Headings,
//! paragraphs, list markers and every code line are selection leaves, so one selection runs
//! across all of them inside the conversation's (or a reader's) selection area; tables (drawn
//! here for their thin scrollbar) and quotes reach them through `paragraph`. Rules and images keep
//! iced's defaults.
use iced::widget::markdown::{self, Bullet, HeadingLevel, Item, Settings, Text, Uri, Viewer};
use iced::widget::{button, checkbox, column, container, hover, right, row, scrollable, text};
use iced::{Element, Fill, Font, Padding, Pixels, alignment, padding};

use crate::Message;
use crate::selection;
use crate::style::{self, P};

pub struct SillViewer<'a> {
    pub media: Option<(&'a str, &'a crate::media::Cache)>,
}

fn bullet_items(bullet: &Bullet) -> &[Item] {
    match bullet {
        Bullet::Point { items } | Bullet::Task { items, .. } => items,
    }
}

impl<'a> Viewer<'a, Message> for SillViewer<'a> {
    fn image(
        &self,
        _settings: Settings,
        url: &'a Uri,
        _title: &'a str,
        _alt: &Text,
    ) -> Element<'a, Message> {
        match self.media {
            Some((session, cache)) => crate::media::picture(cache, session, &url.to_string()),
            None => text(url.to_string()).into(),
        }
    }
    fn on_link_click(url: Uri) -> Message {
        Message::Link(url)
    }

    /// As iced_selection's heading: the level's size, half a line above all but the first.
    fn heading(
        &self,
        settings: Settings,
        level: &'a HeadingLevel,
        text: &'a Text,
        index: usize,
    ) -> Element<'a, Message> {
        let size = match level {
            HeadingLevel::H1 => settings.h1_size,
            HeadingLevel::H2 => settings.h2_size,
            HeadingLevel::H3 => settings.h3_size,
            HeadingLevel::H4 => settings.h4_size,
            HeadingLevel::H5 => settings.h5_size,
            HeadingLevel::H6 => settings.h6_size,
        };
        container(
            selection::rich_text(text.spans(settings.style))
                .on_link_click(Message::Link)
                .on_link_copy(Message::Copy)
                .size(size)
                .style(style::selectable_inherited),
        )
        .id(format!(
            "{}-heading-{index}",
            if self.media.is_none() {
                "markdown"
            } else {
                "conversation"
            }
        ))
        .padding(padding::top(if index > 0 {
            settings.text_size / 2.0
        } else {
            Pixels::ZERO
        }))
        .into()
    }

    /// Inline code a size smaller than the prose: the code face is wider than the prose face,
    /// so code is set one step down (92 %).
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
        selection::rich_text(spans)
            .size(settings.text_size)
            .on_link_click(Message::Link)
            .on_link_copy(Message::Copy)
            .style(style::selectable_inherited)
            .into()
    }

    /// As iced_selection's list: a bullet (or a task's box) beside each item's blocks.
    fn unordered_list(&self, settings: Settings, bullets: &'a [Bullet]) -> Element<'a, Message> {
        column(bullets.iter().map(|bullet| {
            let marker: Element<'a, Message> = match bullet {
                Bullet::Point { .. } => selection::text("•")
                    .size(settings.text_size)
                    .style(style::selectable_inherited)
                    .into(),
                Bullet::Task { done, .. } => container(checkbox(*done).size(settings.text_size))
                    .center_y(
                        iced::widget::text::LineHeight::default().to_absolute(settings.text_size),
                    )
                    .into(),
            };
            row![marker, self.items(settings, bullet)]
                .spacing(settings.spacing)
                .into()
        }))
        .spacing(settings.spacing * 0.75)
        .padding([0.0, settings.spacing.0])
        .into()
    }

    /// As iced_selection's list: right-aligned numbers wide enough for the longest.
    fn ordered_list(
        &self,
        settings: Settings,
        start: u64,
        bullets: &'a [Bullet],
    ) -> Element<'a, Message> {
        let digits = ((start + bullets.len() as u64).max(1) as f32)
            .log10()
            .ceil();
        column(bullets.iter().enumerate().map(|(i, bullet)| {
            row![
                selection::text(format!("{}.", i as u64 + start))
                    .size(settings.text_size)
                    .align_x(alignment::Horizontal::Right)
                    .width(settings.text_size * ((digits / 2.0).ceil() + 1.0))
                    .style(style::selectable_inherited),
                self.items(settings, bullet)
            ]
            .spacing(settings.spacing)
            .into()
        }))
        .spacing(settings.spacing * 0.75)
        .into()
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
                    selection::rich_text(line.spans(settings.style))
                        .on_link_click(Message::Link)
                        .on_link_copy(Message::Copy)
                        .font(Font::with_name(crate::fonts::MONO_FAMILY))
                        .size(settings.code_size)
                        .style(style::selectable_inherited)
                        .into()
                })))
                .padding(Padding::from([8.0, 12.0])),
            )
            .direction(scrollable::Direction::Horizontal(crate::style::bar()))
            .style(style::feed_scroll),
        )
        .width(Fill)
        .style(style::card(P.code_bg));
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

    /// iced's table, scrolling sideways on the thin bar that hides at rest (iced's own is 10 px
    /// with a visible track). Needs the patched `Row::cells` (vendor/README.md).
    fn table(
        &self,
        settings: Settings,
        columns: &'a [markdown::Column],
        rows: &'a [markdown::Row],
    ) -> Element<'a, Message> {
        use iced::widget::table;
        let table = table(
            columns.iter().enumerate().map(move |(i, column)| {
                table::column(
                    markdown::items(self, settings, &column.header),
                    move |row: &markdown::Row| match row.cells().get(i) {
                        Some(cells) => markdown::items(self, settings, cells),
                        None => text("").into(),
                    },
                )
                // the parser's alignment, by name: iced does not re-export its type
                .align_x(match format!("{:?}", column.alignment).as_str() {
                    "Center" => alignment::Horizontal::Center,
                    "Right" => alignment::Horizontal::Right,
                    _ => alignment::Horizontal::Left,
                })
            }),
            rows,
        )
        .padding_x(settings.spacing.0)
        .padding_y(settings.spacing.0 / 2.0)
        .separator_x(0);
        scrollable(table)
            .direction(scrollable::Direction::Horizontal(crate::style::bar()))
            .spacing(settings.spacing.0 / 2.0)
            .style(style::feed_scroll)
            .into()
    }
}

impl<'a> SillViewer<'a> {
    /// A list item's own blocks, a little closer together than the list's.
    fn items(&self, settings: Settings, bullet: &'a Bullet) -> Element<'a, Message> {
        markdown::view_with(
            bullet_items(bullet),
            Settings {
                spacing: settings.spacing * 0.6,
                ..settings
            },
            self,
        )
    }
}
