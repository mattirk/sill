//! The reader: the open file's bar, its source editor, Markdown preview or diff.
use iced::widget::{
    button, column, container, markdown, responsive, row, rule, scrollable, stack, text,
    text_editor,
};
use iced::{Alignment, Element, Fill, Font, Padding};

use super::ui::{action, glyph_action, msg, tip};
use crate::{
    App, Message, SessionView, keys, selection,
    style::{self, P},
    viewer,
    files::Action,
};

impl App {
    pub(crate) fn file_view<'a>(&'a self, view: &'a SessionView) -> Option<Element<'a, Message>> {
        let state = &view.files;
        let file = state.reader.as_ref()?;
        let rendering = view.reader.as_ref()?;
        let is_diff = file.repo.is_some();
        // one bar (reference 10, 11): the path — `git diff: <path>` for a
        // diff — dimmed, shortened with ~ and cut at its start; then what can be done; close
        let shown = home_relative(&file.path.display);
        let title = if is_diff {
            format!("git diff: {shown}")
        } else {
            shown
        };
        let mut heading = row![
            container(
                iced_selection::text(title)
                    .size(style::TEXT)
                    .wrapping(text::Wrapping::None)
                    .style(style::selectable(P.fg_dim))
            )
            .width(Fill)
            .clip(true),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        if !is_diff {
            heading = heading
                .push(glyph_action("arrow-left", "Back", Action::Back(false)))
                .push(glyph_action("arrow-right", "Forward", Action::Back(true)));
        }
        if is_diff {
            heading = heading.push(action(
                "Open file",
                Action::Open {
                    path: file.path.clone(),
                    repo: None,
                },
            ));
        } else if !state.pinned(&file.path) {
            heading = heading.push(glyph_action(
                "pin",
                "Pin",
                Action::Pin {
                    path: file.path.clone(),
                    on: true,
                },
            ));
        }
        if file.renderable() {
            heading = heading.push(if file.source {
                glyph_action("eye", "Preview", Action::Source)
            } else {
                glyph_action("code", "Source", Action::Source)
            });
        }
        if rendering.dirty {
            heading = heading.push(text("Unsaved").size(style::SMALL).color(P.warning));
        }
        if !is_diff && (file.source || rendering.dirty) {
            heading = heading.push(keys::press_maybe(
                button(
                    text(if rendering.saving.is_some() {
                        "Saving…"
                    } else {
                        "Save"
                    })
                    .size(style::SMALL),
                )
                .padding([3, 7])
                .style(style::quiet),
                (rendering.dirty && rendering.saving.is_none()).then(|| msg(Action::SaveFile)),
            ));
        }
        heading = heading
            .push(glyph_action(
                "refresh",
                "Read the file again",
                Action::RefreshFile,
            ))
            .push(tip(
                keys::press(
                    button(crate::icons::glyph("copy", 13.0, style::alpha(P.fg, 0.7)))
                        .padding([5, 7])
                        .style(style::quiet),
                    Message::CopyFile,
                ),
                "Copy the text".into(),
            ))
            .push(container(glyph_action("x", "Close", Action::CloseViewer)).id("close-file"));
        let body: Element<'_, Message> = if let Some(error) = &file.error {
            container(text(error).size(style::TEXT).color(P.error))
                .padding(18)
                .height(Fill)
                .into()
        } else if file.pending.is_some() {
            container(text("Reading…").size(style::TEXT).color(P.fg_dim))
                .padding(18)
                .height(Fill)
                .into()
        } else if file.source {
            let digits = rendering.content.line_count().max(1).ilog10() + 1;
            let width =
                (rendering.source_columns as f32 + digits.max(2) as f32 + 4.0) * style::MONO * 0.6
                    + 24.0;
            let marks = self.find.source_marks();
            responsive(move |size| {
                let marks = marks.clone();
                let editor = text_editor(&rendering.content)
                    .id("file-reader")
                    .height(Fill)
                    .width(width.max(size.width))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .padding(12)
                    .size(style::MONO)
                    .font(Font::with_name(crate::fonts::MONO_FAMILY))
                    .style(|theme, status| {
                        let mut style = style::reader(theme, status);
                        style.border = iced::Border::default();
                        style
                    })
                    .line_numbers(|editor| {
                        let mut previous = None;
                        editor
                            .buffer()
                            .layout_runs()
                            .filter_map(|run| {
                                let first = previous != Some(run.line_i);
                                previous = Some(run.line_i);
                                (first && run.glyphs.first().is_none_or(|glyph| glyph.start == 0))
                                    .then_some((run.line_i + 1, run.line_top))
                            })
                            .collect()
                    })
                    .marks(move |editor| crate::find::mark_rects(editor.buffer(), &marks))
                    .on_action(|a| msg(Action::Text(a)))
                    .key_binding(crate::ui::reading_keys)
                    .highlight(
                        if is_diff {
                            "diff"
                        } else {
                            crate::syntax::token(&file.path.display)
                        },
                        crate::syntax::theme(),
                    );
                scrollable(editor)
                    .id(format!("source-horizontal-{}", rendering.key))
                    .direction(scrollable::Direction::Horizontal(style::bar()))
                    .width(Fill)
                    .height(Fill)
                    .style(style::feed_scroll)
                    .into()
            })
            .into()
        } else if is_diff {
            let diff = crate::diff_ui::diff_rows(&file.text, &file.path.display);
            // the reader's text is one row for Find (`find::reader_leaves`)
            selection::area(
                scrollable(crate::rows::scoped(0, container(diff).padding(12)))
                    .id("diff-reader")
                    .height(Fill)
                    .direction(scrollable::Direction::Both {
                        vertical: style::bar(),
                        horizontal: style::bar(),
                    })
                    .style(style::feed_scroll),
            )
            .on_copy(Message::Copy)
            .selection(&self.find.reader_selected)
            .find(self.find.reader_query())
            .visible_leaves(&self.find.visible_leaves)
            .into()
        } else if let Some(markdown) = &rendering.markdown {
            selection::area(
                scrollable(crate::rows::scoped(
                    0,
                    container(
                        markdown::view_with(
                            markdown.items(),
                            crate::ui::markdown_settings(),
                            &viewer::SillViewer { media: None },
                        )
                        .map({
                            let session = view.id.clone();
                            let source = file.path.clone();
                            move |message| match message {
                                Message::Link(target) => msg(Action::Link {
                                    session: session.clone(),
                                    source: source.clone(),
                                    target,
                                }),
                                other => other,
                            }
                        }),
                    )
                    .padding(16),
                ))
                .direction(scrollable::Direction::Vertical(style::bar()))
                .id("markdown-reader")
                .height(Fill)
                .style(style::feed_scroll),
            )
            .on_copy(Message::Copy)
            .selection(&self.find.reader_selected)
            .find(self.find.reader_query())
            .visible_leaves(&self.find.visible_leaves)
            .into()
        } else {
            container(text("Reading…").size(style::TEXT).color(P.fg_dim))
                .padding(18)
                .height(Fill)
                .into()
        };
        let mut page = column![
            container(heading).padding(Padding {
                top: 4.0,
                bottom: 4.0,
                left: 12.0,
                right: 6.0,
            }),
            rule::horizontal(1).style(style::rule_style(P.border)),
        ];
        if let Some(notice) = &rendering.notice {
            let mut notice_row = row![text(notice).size(style::SMALL).width(Fill)]
                .spacing(8)
                .align_y(Alignment::Center);
            if self
                .drawer
                .pending_discard
                .as_ref()
                .is_some_and(|(id, _)| id == &view.id)
            {
                notice_row = notice_row
                    .push(action("Discard", Action::DiscardEdits))
                    .push(action("Cancel", Action::CancelDiscard));
            }
            page = page.push(
                container(notice_row)
                    .padding([8, 12])
                    .style(style::fill(style::alpha(P.warning, 0.10))),
            );
        }
        // the body stays the stack's first layer with the bar open or closed, so the editor and
        // the scrollables keep their state (where they are scrolled to, the cursor)
        let finding = self.find.shown
            && matches!(self.find.surface, Some(crate::find::Surface::Reader { .. }));
        page = page.push(if finding {
            stack![body, self.find.bar()]
        } else {
            stack![body]
        });
        if is_diff {
            page = page.push(
                container(
                    text("git's view: the working tree against HEAD (open the file from the tree to edit it)")
                        .size(style::TINY)
                        .color(P.fg_dim),
                )
                .padding([6, 12]),
            );
        }
        Some(page.height(Fill).into())
    }
}


/// A path as the reader titles it: the home directory as `~`.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
