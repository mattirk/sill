//! Draft annotation transactions. Only a successful Save replaces attachment bytes.
use crate::{
    App, Message, SessionView,
    annotation::{self, Editor, Event, Saved},
    media,
};
use iced::Task;
use sill_proto::sfp::Attachment;
use std::collections::BTreeSet;

const PIXEL_BUDGET: usize = 128 * 1024 * 1024;

fn inline_bytes(a: &Attachment) -> usize {
    match a {
        Attachment::Inline { data_base64, .. } => {
            data_base64.len() / 4 * 3 - data_base64.bytes().rev().take_while(|b| *b == b'=').count()
        }
        _ => 0,
    }
}

impl SessionView {
    /// Count shared decoded originals once, including those held by pending submissions.
    pub(crate) fn decoded_media_bytes(&self) -> usize {
        let mut seen = BTreeSet::new();
        let mut bytes = 0;
        let mut add = |pic: &media::Picture| {
            if seen.insert(std::sync::Arc::as_ptr(&pic.rgba) as usize) {
                bytes += pic.rgba.len();
            }
        };
        for pic in self
            .media
            .values()
            .filter_map(|v| v.as_ref()?.as_ref().ok())
        {
            add(pic);
        }
        for a in self
            .attachments
            .iter()
            .chain(self.pending_attachments.values().flatten())
        {
            if let Some(saved) = &a.annotation {
                add(&saved.picture);
            }
        }
        if let Some(editor) = &self.annotation {
            add(&editor.picture);
        }
        bytes
    }

    pub(crate) fn prune_draft_media(&mut self) {
        let keep: BTreeSet<_> = self
            .attachments
            .iter()
            .chain(self.pending_attachments.values().flatten())
            .map(|a| media::key(a))
            .collect();
        self.media
            .retain(|key, _| !key.starts_with("draft:") || keep.contains(key));
    }
}

impl App {
    pub(crate) fn annotation_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Scoped(revision, event) => {
                if self
                    .current()
                    .and_then(|v| v.annotation.as_ref())
                    .is_some_and(|e| e.revision == revision)
                {
                    return self.annotation_event(*event);
                }
            }
            Event::Open(id) => {
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                if view.annotation.is_some() || view.attachments_busy {
                    return Task::none();
                }
                let Some(a) = view.attachments.iter().find(|a| a.id == id) else {
                    return Task::none();
                };
                if !media::is_image(a) {
                    return Task::none();
                }
                let (original, picture, strokes) = if let Some(saved) = &a.annotation {
                    (
                        saved.original.clone(),
                        saved.picture.clone(),
                        saved.strokes.clone(),
                    )
                } else {
                    let Some(Some(Ok(pic))) = view.media.get(&media::key(a)) else {
                        return Task::none();
                    };
                    (a.attachment.clone(), pic.clone(), vec![])
                };
                view.annotation = Some(Editor {
                    attachment_id: id,
                    revision: sill_client::request_id("annotation"),
                    original,
                    picture,
                    strokes,
                    saving: false,
                    drawing: false,
                    error: None,
                });
            }
            Event::Drawing(drawing) => {
                if let Some(editor) = self.current_mut().and_then(|v| v.annotation.as_mut()) {
                    editor.drawing = drawing;
                }
            }
            Event::Stroke(stroke) => {
                if let Some(editor) = self.current_mut().and_then(|v| v.annotation.as_mut())
                    && !editor.saving
                {
                    editor.drawing = false;
                    editor.strokes.push(stroke);
                    editor.error = None;
                }
            }
            Event::Undo | Event::Clear => {
                if let Some(editor) = self.current_mut().and_then(|v| v.annotation.as_mut())
                    && !editor.saving
                {
                    if editor.drawing {
                        return Task::none();
                    }
                    if matches!(event, Event::Clear) {
                        editor.strokes.clear();
                    } else {
                        editor.strokes.pop();
                    }
                    editor.error = None;
                }
            }
            Event::Cancel => {
                if let Some(view) = self.current_mut() {
                    view.annotation = None;
                }
                return self.restore_scroll().chain(self.focus_prompt());
            }
            Event::Save => {
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                let Some(editor) = view.annotation.as_mut() else {
                    return Task::none();
                };
                if editor.saving || editor.drawing {
                    return Task::none();
                }
                editor.saving = true;
                editor.error = None;
                let snapshot = editor.clone();
                let session = view.id.clone();
                let token = view.media_token.clone();
                let attachment_id = editor.attachment_id.clone();
                let revision = editor.revision.clone();
                return crate::blocking(
                    move || {
                        annotation::render(&snapshot.original, &snapshot.picture, &snapshot.strokes)
                    },
                    move |result| {
                        Message::Annotation(Event::Saved {
                            session: session.clone(),
                            token: token.clone(),
                            attachment_id: attachment_id.clone(),
                            revision: revision.clone(),
                            result,
                        })
                    },
                );
            }
            Event::Saved {
                session,
                token,
                attachment_id,
                revision,
                result,
            } => {
                let Some(view) = self.views.get_mut(&session) else {
                    return Task::none();
                };
                if view.media_token != token
                    || !view.annotation.as_ref().is_some_and(|e| {
                        e.attachment_id == attachment_id && e.revision == revision && e.saving
                    })
                {
                    return Task::none();
                }
                let Some(index) = view.attachments.iter().position(|a| a.id == attachment_id)
                else {
                    view.annotation = None;
                    return Task::none();
                };
                let result = result.and_then(|(a, pic)| {
                    let total = view.attachments.iter().enumerate()
                        .map(|(i, old)| inline_bytes(if i == index { &a } else { &old.attachment })).sum::<usize>();
                    if total > sill_proto::initialize::MAX_INLINE_BYTES {
                        return Err("Images together exceed 10 MiB; cancel and remove an image first.".into());
                    }
                    // The replaced flattened preview can be released, unless another draft/pending
                    // attachment references it. Original pixels remain owned by the editor.
                    let old_key = media::key(&view.attachments[index]);
                    let old_pic = view.media.get(&old_key).and_then(|p| p.as_ref()?.as_ref().ok());
                    let shared = view.attachments.iter().enumerate().any(|(i, a)| i != index && media::key(a) == old_key)
                        || view.pending_attachments.values().flatten().any(|a| media::key(a) == old_key);
                    let new_key = media::key(&a);
                    let released_key = !shared && old_key != new_key;
                    if view.media.len() + usize::from(!view.media.contains_key(&new_key))
                        - usize::from(released_key && view.media.contains_key(&old_key)) > 64 {
                        return Err("Session image cache is full (64 images). Cancel and remove an image first.".into());
                    }
                    let freed = old_pic.filter(|old| !shared && !std::sync::Arc::ptr_eq(&old.rgba, &view.annotation.as_ref().unwrap().picture.rgba))
                        .map_or(0, |p| p.rgba.len());
                    let additional = if std::sync::Arc::ptr_eq(&pic.rgba, &view.annotation.as_ref().unwrap().picture.rgba) { 0 } else { pic.rgba.len() };
                    if view.decoded_media_bytes().saturating_sub(freed) + additional > PIXEL_BUDGET {
                        return Err("Annotation exceeds the session image limit (128 MiB). Cancel and remove an image first.".into());
                    }
                    Ok((a, pic))
                });
                match result {
                    Ok((attachment, picture)) => {
                        let editor = view.annotation.take().unwrap();
                        view.attachments[index].annotation = (!editor.strokes.is_empty())
                            .then_some(Saved {
                                original: editor.original,
                                picture: editor.picture,
                                strokes: editor.strokes,
                            });
                        let key = media::key(&attachment);
                        view.attachments[index].attachment = attachment;
                        view.media.insert(key, Some(Ok(picture)));
                        view.prune_draft_media();
                        if self.selected.as_deref() == Some(&session) {
                            return self.restore_scroll().chain(self.focus_prompt());
                        }
                    }
                    Err(why) => {
                        let editor = view.annotation.as_mut().unwrap();
                        editor.saving = false;
                        editor.error = Some(why);
                    }
                }
            }
        }
        Task::none()
    }
}
