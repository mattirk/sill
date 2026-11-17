//! Links in the reader: a document's links resolved against the document, Back/Forward per
//! session, and the jump to a heading once the reader is laid out.
use iced::Task;
use iced::widget::markdown;
use sill_client::files;
use sill_proto::sessions::HostPath;

use super::Action;
use crate::{App, Message};

#[derive(Default)]
pub(super) struct Navigation {
    pub(super) back: Vec<HostPath>,
    pub(super) forward: Vec<HostPath>,
    pub(super) anchor: Option<(String, String)>,
}

impl App {
    pub(crate) fn conversation_file_link(&mut self, target: String) -> Task<Message> {
        let Some(session) = self.selected.clone() else {
            return Task::none();
        };
        // Conversation file links are absolute host paths, read through the session's
        // file worker (including workspace aliases), never the GUI's local filesystem.
        let source = HostPath::from_path(std::path::Path::new("/"));
        if let Ok(LinkTarget::File(path, fragment)) = resolve_link(&source, &target) {
            return self.update(Message::Files(Action::Navigate { session, path, fragment, history: true }));
        }
        Task::none()
    }

    pub(super) fn follow_link(&mut self, session: String, source: HostPath, target: String) -> Task<Message> {
        if !self.views.get(&session).and_then(|v| v.files.reader.as_ref()).is_some_and(|f| files::key(&f.path) == files::key(&source)) {
            return Task::none();
        }
        match resolve_link(&source, &target) {
            Ok(LinkTarget::Web(url)) => {
                crate::terminal::open_link(url);
                Task::none()
            }
            Ok(LinkTarget::File(path, fragment)) => self.update(Message::Files(Action::Navigate { session, path, fragment, history: true })),
            Err(error) => {
                if let Some(view) = self.views.get_mut(&session)
                    && let Some(reader) = &mut view.reader
                {
                    reader.notice = Some(error);
                }
                Task::none()
            }
        }
    }

    pub(super) fn navigate_back(&mut self, forward: bool) -> Task<Message> {
        let Some(session) = self.selected.clone() else {
            return Task::none();
        };
        let nav = self.drawer.navigation.entry(session.clone()).or_default();
        let path = if forward { nav.forward.pop() } else { nav.back.pop() };
        let Some(path) = path else {
            return Task::none();
        };
        if let Some(file) = self.views.get(&session).and_then(|v| v.files.reader.as_ref()) {
            if forward {
                nav.back.push(file.path.clone());
            } else {
                nav.forward.push(file.path.clone());
            }
        }
        self.files_action(Action::Navigate { session, path, fragment: String::new(), history: false })
    }

    pub(super) fn navigate(&mut self, session: String, path: HostPath, fragment: String, history: bool) -> Task<Message> {
        let Some(view) = self.views.get_mut(&session) else {
            return Task::none();
        };
        let same = view.files.reader.as_ref().is_some_and(|f| files::key(&f.path) == files::key(&path));
        let nav = self.drawer.navigation.entry(session.clone()).or_default();
        nav.anchor = Some((files::key(&path).into(), fragment));
        if !same {
            if history {
                if let Some(file) = &view.files.reader {
                    nav.back.push(file.path.clone());
                }
                nav.forward.clear();
            }
            let request = view.files.open(path, None);
            view.sync_reader();
            self.files_send(&session, vec![request]);
            self.reader_anchor(&session)
        } else {
            self.reader_anchor(&session)
        }
    }

    pub(super) fn anchor_offset(&mut self, session: String, key: String, y: f32) -> Task<Message> {
        if self.selected.as_deref() == Some(&session) && self.views.get(&session).and_then(|v| v.reader.as_ref()).is_some_and(|r| r.key == key) {
            iced::widget::operation::scroll_to("markdown-reader", iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(y) })
        } else {
            Task::none()
        }
    }

    pub(crate) fn reader_anchor(&mut self, session: &str) -> Task<Message> {
        if self.selected.as_deref() != Some(session) {
            return Task::none();
        }
        let Some(nav) = self.drawer.navigation.get_mut(session) else {
            return Task::none();
        };
        let Some((key, fragment)) = nav.anchor.as_ref() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        let Some(file) = &view.files.reader else {
            return Task::none();
        };
        if files::key(&file.path) != key || file.pending.is_some() {
            return Task::none();
        }
        let Some(reader) = &mut view.reader else {
            return Task::none();
        };
        let Some(markdown) = &reader.markdown else {
            return Task::none();
        };
        let key = key.clone();
        let fragment = fragment.clone();
        nav.anchor = None;
        if fragment.is_empty() {
            return iced::widget::operation::scroll_to("markdown-reader", iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(0.0) });
        }
        let Some(index) = heading_index(markdown.items(), &fragment) else {
            reader.notice = Some(format!("Heading not found: #{fragment}"));
            return Task::none();
        };
        iced_runtime::task::widget(FindHeading {
            target: iced::advanced::widget::Id::from(format!("markdown-heading-{index}")),
            session: session.into(),
            key,
            origin: 0.0,
            y: None,
        })
    }
}

#[derive(Debug)]
pub(super) enum LinkTarget {
    Web(String),
    File(HostPath, String),
}

pub(super) fn resolve_link(source: &HostPath, target: &str) -> Result<LinkTarget, String> {
    if target.starts_with("https://") || target.starts_with("http://") {
        return Ok(LinkTarget::Web(target.into()));
    }
    if target.starts_with("//") || target.split('/').next().unwrap_or("").contains(':') {
        return Err(format!("Unsupported link: {target}"));
    }
    let (path, fragment) = target.split_once('#').unwrap_or((target, ""));
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    use std::os::unix::ffi::OsStringExt;
    let decoded = percent_encoding::percent_decode_str(path).collect::<Vec<_>>();
    if decoded.contains(&0) {
        return Err("Link contains a NUL byte".into());
    }
    let source = source.to_path().map_err(|e| e.to_string())?;
    let path = if path.is_empty() {
        source.clone()
    } else {
        let decoded = std::path::PathBuf::from(std::ffi::OsString::from_vec(decoded));
        if decoded.is_absolute() { decoded } else { source.parent().ok_or("Document has no parent directory")?.join(decoded) }
    };
    // Normalize lexically: the daemon resolves and reads host paths, never this GUI process.
    let mut normalized = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    let fragment = percent_encoding::percent_decode_str(fragment).decode_utf8().map_err(|_| "Heading link is not UTF-8")?.into_owned();
    Ok(LinkTarget::File(HostPath::from_path(&normalized), fragment))
}

pub(super) fn heading_index(items: &[markdown::Item], fragment: &str) -> Option<usize> {
    let mut used = std::collections::BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        if let markdown::Item::Heading(_, text) = item {
            let text = text.spans(crate::ui::markdown_settings().style).iter().map(|span| span.text.as_ref()).collect::<String>();
            let slug: String = text
                .to_lowercase()
                .chars()
                .filter_map(|c| {
                    if c.is_alphanumeric() || c == '_' || c == '-' {
                        Some(c)
                    } else if c.is_whitespace() {
                        Some('-')
                    } else {
                        None
                    }
                })
                .collect();
            let mut anchor = slug.clone();
            let mut suffix = 0;
            while used.contains(&anchor) {
                suffix += 1;
                anchor = format!("{slug}-{suffix}");
            }
            used.insert(anchor.clone());
            if anchor == fragment {
                return Some(index);
            }
        }
    }
    None
}

pub(super) struct FindHeading {
    pub(super) target: iced::advanced::widget::Id,
    pub(super) session: String,
    pub(super) key: String,
    pub(super) origin: f32,
    pub(super) y: Option<f32>,
}
impl iced::advanced::widget::operation::Operation<Message> for FindHeading {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::operation::Operation<Message>)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        content: iced::Rectangle,
        _translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&iced::advanced::widget::Id::from("markdown-reader")) {
            self.origin = content.y;
        }
    }
    fn container(&mut self, id: Option<&iced::advanced::widget::Id>, bounds: iced::Rectangle) {
        if id == Some(&self.target) {
            self.y = Some((bounds.y - self.origin).max(0.0));
        }
    }
    fn finish(&self) -> iced::advanced::widget::operation::Outcome<Message> {
        match self.y {
            Some(y) => iced::advanced::widget::operation::Outcome::Some(Message::Files(Action::AnchorOffset {
                session: self.session.clone(),
                key: self.key.clone(),
                y,
            })),
            None => iced::advanced::widget::operation::Outcome::None,
        }
    }
}
