//! Session-owned drafts, image fetches and clipboard/file attachment input.
use crate::{App, Message, settings_ui::quiet_button, style};
use iced::widget::{button, column, container, hover, image, text};
use iced::{Element, Task};
use sill_proto::{
    admin,
    sfp::{Attachment, Upload},
};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex, OnceLock},
};
const LIMIT: usize = sill_proto::initialize::MAX_INLINE_BYTES;
#[derive(Debug, Clone)]
pub struct Picture {
    pub handle: image::Handle,
    pub(crate) rgba: Arc<Vec<u8>>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}
/// A local occurrence, separate from the bytes sent to the daemon (duplicates have distinct IDs).
#[derive(Debug, Clone)]
pub struct DraftAttachment {
    pub id: String,
    pub attachment: Attachment,
    pub annotation: Option<crate::annotation::Saved>,
}
impl From<Attachment> for DraftAttachment {
    fn from(attachment: Attachment) -> Self {
        Self { id: sill_client::request_id("draft-image"), attachment, annotation: None }
    }
}
impl std::ops::Deref for DraftAttachment {
    type Target = Attachment;
    fn deref(&self) -> &Attachment { &self.attachment }
}
pub type Cache = BTreeMap<String, Option<Result<Picture, String>>>;
#[derive(Debug, Clone)]
pub enum Event {
    /// Async work targets the exact view that started it, including after a tab is reopened.
    Scoped(String, String, Box<Event>),
    Choose,
    Paste,
    Remove(usize),
    Attached(String, Result<Vec<(Attachment, Option<Picture>)>, String>),
    Pasted(String, Result<Paste, String>),
    Loaded(String, String, Result<Picture, String>),
    Reload(String, String),
    Fetched(String, String, String, Result<Picture, String>),
    Open(String, String),
    Close,
    Fullscreen(String),
    FullscreenReady(String, iced::window::Id, iced::window::Mode),

    Copy(String, String),
    Copied(String, Result<(), String>),
}
#[derive(Debug, Clone)]
pub enum Paste {
    Text(String),
    Image(Vec<(Attachment, Option<Picture>)>),
}
fn clipboard<T>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, String>) -> Result<T, String> {
    static CLIP: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    let mut clip = CLIP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| e.to_string())?;
    if clip.is_none() {
        *clip = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    f(clip.as_mut().unwrap())
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Picture, String> {
    let (w, h) = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())?;
    if u64::from(w) * u64::from(h) * 4 > 32 * 1024 * 1024 {
        return Err("Image exceeds the decoded preview limit (32 MiB).".into());
    }
    let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = ::image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let rgba = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = rgba.dimensions();
    if rgba.len() > 32 * 1024 * 1024 {
        return Err("Image exceeds the preview size limit.".into());
    }
    let rgba = Arc::new(rgba.into_raw());
    Ok(Picture {
        handle: image::Handle::from_rgba(width, height, rgba.as_ref().clone()),
        rgba,
        width,
        height,
    })
}
pub fn name(a: &Attachment) -> &str {
    match a {
        Attachment::Inline { name, .. } | Attachment::Uploaded { name, .. } => name,
    }
}
pub fn key(a: &Attachment) -> String {
    match a {
        Attachment::Uploaded { upload_id, .. } => format!("upload:{upload_id}"),
        Attachment::Inline {
            name, data_base64, ..
        } => {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            data_base64.hash(&mut hash);
            format!("draft:{name}:{:x}", hash.finish())
        }
    }
}
fn make_image(
    name: String,
    bytes: Vec<u8>,
    mime: &str,
) -> Result<(Attachment, Option<Picture>), String> {
    if bytes.len() > LIMIT {
        return Err("Images are limited to 10 MiB per submission.".into());
    }
    let picture = decode(&bytes)?;
    Ok((
        Attachment::Inline {
            name,
            mime: mime.into(),
            data_base64: admin::encode_bytes(&bytes),
        },
        Some(picture),
    ))
}
fn attach(
    path: std::path::PathBuf,
    session: &str,
    actions: &sill_client::actions::Actions,
) -> Result<(Attachment, Option<Picture>), String> {
    if !path.is_file() {
        return Err("Choose a regular file.".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(|e| e.to_string())?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > LIMIT {
        return Err(format!("{} exceeds 10 MiB.", path.display()));
    }
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mime = match ::image::guess_format(&bytes).ok() {
        Some(::image::ImageFormat::Png) => Some("image/png"),
        Some(::image::ImageFormat::Jpeg) => Some("image/jpeg"),
        Some(::image::ImageFormat::Gif) => Some("image/gif"),
        Some(::image::ImageFormat::WebP) => Some("image/webp"),
        _ => None,
    };
    if let Some(mime) = mime {
        return make_image(name, bytes, mime);
    }
    let upload_id = sill_client::request_id("attachment");
    let params = sill_proto::sfp::UploadCreate {
        session_id: session.into(),
        upload_id: upload_id.clone(),
        name: name.clone(),
        mime: "application/octet-stream".into(),
        data_base64: admin::encode_bytes(&bytes),
    };
    match actions.call(
        "_sill/uploads.create",
        serde_json::to_value(params).unwrap(),
    ) {
        sill_client::actions::Outcome::Done(_) => Ok((
            Attachment::Uploaded {
                upload_id,
                name,
                mime: "application/octet-stream".into(),
            },
            None,
        )),
        other => Err(format!(
            "Upload did not complete: {}",
            other.problem().unwrap_or("unknown response")
        )),
    }
}
// Consume uploads/decodes lazily so a compressed multi-image selection cannot retain
// gigabytes of decoded previews before the UI checks its session cache budget.
fn collect_attachments(
    files: impl IntoIterator<Item = Result<(Attachment, Option<Picture>), String>>,
    budget: usize,
) -> Result<Vec<(Attachment, Option<Picture>)>, String> {
    let mut remaining = budget;
    let mut collected = Vec::new();
    for file in files {
        let file = file?;
        let size = file.1.as_ref().map_or(0, |p| p.rgba.len());
        remaining = remaining
            .checked_sub(size)
            .ok_or("Attachment preview limit reached; remove attachments first.")?;
        collected.push(file);
    }
    Ok(collected)
}

/// Small attachment preview; controls belong to its surrounding strip.
pub fn thumbnail<'a>(cache: &Cache, key: &str, width: f32, height: f32) -> Element<'a, Message> {
    let content: Element<'a, Message> = match cache.get(key) {
        Some(Some(Ok(pic))) => image(pic.handle.clone())
            .width(width)
            .height(height)
            .content_fit(iced::ContentFit::Contain)
            .into(),
        Some(Some(Err(_))) => text("!").size(style::SMALL).color(style::P.error).into(),
        _ => text("…").size(style::SMALL).color(style::P.fg_dim).into(),
    };
    iced::widget::container(content)
        .width(width)
        .height(height)
        .center_x(width)
        .center_y(height)
        .style(|_| iced::widget::container::Style {
            background: Some(style::P.code_bg.into()),
            border: iced::Border {
                color: style::alpha(style::P.fg, 0.12),
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .clip(true)
        .into()
}
pub fn is_image(a: &Attachment) -> bool {
    match a {
        Attachment::Inline { mime, .. } | Attachment::Uploaded { mime, .. } => {
            mime.starts_with("image/")
        }
    }
}

pub fn picture<'a>(cache: &Cache, session: &str, key: &str) -> Element<'a, Message> {
    match cache.get(key) {
        Some(Some(Ok(pic))) => {
            let action = |label: &'static str, event: Event| {
                crate::keys::press(
                    button(text(label).size(style::SMALL))
                        .padding([3, 6])
                        .style(|theme, status| iced::widget::button::Style {
                            background: Some(style::P.code_bg.into()),
                            ..style::quiet(theme, status)
                        }),
                    Message::Media(event),
                )
            };
            let preview = iced::widget::mouse_area(image(pic.handle.clone())
                .width(iced::Fill)
                .height(240)
                .content_fit(iced::ContentFit::Contain))
                .on_press(Message::Media(Event::Open(session.into(), key.into())));
            let controls = container(iced::widget::row![
                action("Expand", Event::Open(session.into(), key.into())),
                action("Copy", Event::Copy(session.into(), key.into())),
            ].spacing(4))
                .padding(6)
                .align_right(iced::Fill);
            container(hover(preview, controls)).max_width(640).into()
        }
        Some(Some(Err(why))) => column![
            text(format!("Image unavailable: {why}")).size(style::SMALL),
            quiet_button(
                "Retry image",
                Some(Message::Media(Event::Reload(session.into(), key.into())))
            )
        ]
        .into(),
        None => text("Image preview unavailable (session cache limit).")
            .size(style::SMALL)
            .into(),
        _ => text("Loading image…").size(style::SMALL).into(),
    }
}
pub fn item_keys(item: &sill_proto::sfp::Item) -> Vec<String> {
    item.content
        .iter()
        .filter(|b| b.kind == "image")
        .filter_map(|b| {
            b.extra
                .get("upload_id")
                .and_then(|v| v.as_str())
                .map(|s| format!("upload:{s}"))
        })
        .collect()
}
impl App {
    pub(crate) fn media_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Open(session, key) => {
                if let Some(view) = self.views.get_mut(&session)
                    && matches!(view.media.get(&key), Some(Some(Ok(_))))
                {
                    view.image_viewer = Some(key);
                }
            }
            Event::Close => {
                if let Some(view) = self.current_mut() {
                    view.image_viewer = None;
                    if let Some((id, mode)) = view.image_viewer_mode.take() {
                        return iced::window::set_mode(id, mode);
                    }
                }
            }
            Event::Fullscreen(session) => {
                return iced::window::latest().and_then(move |id| {
                    let session = session.clone();
                    iced::window::mode(id).map(move |mode|
                        Message::Media(Event::FullscreenReady(session.clone(), id, mode)))
                });
            }
            Event::FullscreenReady(session, id, mode) => {
                if let Some(view) = self.views.get_mut(&session) && view.image_viewer.is_some() {
                    if let Some((id, prior)) = view.image_viewer_mode.take() {
                        return iced::window::set_mode(id, prior);
                    }
                    view.image_viewer_mode = Some((id, mode));
                    return iced::window::set_mode(id, iced::window::Mode::Fullscreen);
                }
            }
            Event::Fetched(session, key, request, result) => {
                if self.views.get(&session).and_then(|v| v.media_requests.get(&key)) == Some(&request) {
                    self.views.get_mut(&session).unwrap().media_requests.remove(&key);
                    return self.media_event(Event::Loaded(session, key, result));
                }
            }
            Event::Scoped(session, token, event) => {
                if self
                    .views
                    .get(&session)
                    .is_some_and(|v| v.media_token == token)
                {
                    return self.media_event(*event);
                }
            }
            Event::Choose | Event::Paste => {
                let paste = matches!(event, Event::Paste);
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                if view.attachments_busy || view.annotation.is_some() {
                    return Task::none();
                }
                view.attachments_busy = true;
                let session = view.id.clone();
                let token = view.media_token.clone();
                let dest = session.clone();
                if paste {
                    return crate::blocking(
                        move || {
                            clipboard(|clip| {
                                if let Ok(pic) = clip.get_image() {
                                    if pic.bytes.len() > 32 * 1024 * 1024 {
                                        return Err(
                                            "Clipboard image exceeds 32 MiB decoded.".into()
                                        );
                                    }
                                    let rgba = ::image::RgbaImage::from_raw(
                                        pic.width as u32,
                                        pic.height as u32,
                                        pic.bytes.into_owned(),
                                    )
                                    .ok_or("Invalid clipboard image")?;
                                    let mut png = std::io::Cursor::new(Vec::new());
                                    ::image::DynamicImage::ImageRgba8(rgba)
                                        .write_to(&mut png, ::image::ImageFormat::Png)
                                        .map_err(|e| e.to_string())?;
                                    Ok(Paste::Image(vec![make_image(
                                        format!("{}.png", sill_client::request_id("clipboard")),
                                        png.into_inner(),
                                        "image/png",
                                    )?]))
                                } else {
                                    clip.get_text().map(Paste::Text).map_err(|e| e.to_string())
                                }
                            })
                        },
                        move |r| {
                            Message::Media(Event::Scoped(
                                dest.clone(),
                                token.clone(),
                                Box::new(Event::Pasted(dest.clone(), r)),
                            ))
                        },
                    );
                }
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title("Attach files")
                            .pick_files()
                            .await
                            .map(|files| {
                                files
                                    .into_iter()
                                    .map(|f| f.path().to_path_buf())
                                    .collect::<Vec<_>>()
                            })
                    },
                    move |paths| Message::AttachmentPaths(session.clone(), token.clone(), paths),
                );
            }
            Event::Remove(index) => {
                if let Some(v) = self.current_mut() {
                    if v.annotation.is_some() { return Task::none(); }
                    if index < v.attachments.len() {
                        let removed = v.attachments.remove(index);
                        let key = key(&removed);
                        if !v.attachments.iter().any(|a| crate::media::key(a) == key)
                            && !v.outbox.pending().iter().any(|p| {
                                p.submit
                                    .attachments
                                    .iter()
                                    .any(|a| crate::media::key(a) == key)
                            })
                        {
                            v.media.remove(&key);
                        }
                    }
                }
            }
            Event::Attached(session, result) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.attachments_busy = false;
                    match result {
                        Ok(files) => {
                            let total: usize = view
                                .attachments
                                .iter().map(|a| &a.attachment)
                                .chain(files.iter().map(|(a, _)| a))
                                .map(|a| match a {
                                    Attachment::Inline { data_base64, .. } => {
                                        data_base64.len() / 4 * 3
                                            - data_base64
                                                .bytes()
                                                .rev()
                                                .take_while(|b| *b == b'=')
                                                .count()
                                    }
                                    _ => 0,
                                })
                                .sum();
                            let used = view.decoded_media_bytes();
                            let added: usize = files
                                .iter()
                                .filter_map(|(_, p)| p.as_ref())
                                .map(|p| p.rgba.len())
                                .sum();
                            let added_keys: std::collections::BTreeSet<_> = files.iter()
                                .filter(|(_, pic)| pic.is_some()).map(|(a, _)| key(a))
                                .filter(|key| !view.media.contains_key(key)).collect();
                            if used + added > 128 * 1024 * 1024
                                || view.media.len() + added_keys.len() > 64
                                || view.attachments.len() + files.len() > 64
                            {
                                view.failed = Some(
                                    "Attachment preview limit reached; remove attachments first."
                                        .into(),
                                );
                            } else if total > LIMIT {
                                view.failed = Some(
                                    "Images together exceed 10 MiB; remove an image first.".into(),
                                );
                            } else {
                                for (a, pic) in files {
                                    if let Some(pic) = pic {
                                        view.media.insert(key(&a), Some(Ok(pic)));
                                    }
                                    view.attachments.push(a.into());
                                }
                            }
                        }
                        Err(why) => view.failed = Some(why),
                    }
                }
            }
            Event::Pasted(session, result) => match result {
                Ok(Paste::Image(files)) => {
                    return self.media_event(Event::Attached(session, Ok(files)));
                }
                other => {
                    if let Some(view) = self.views.get_mut(&session) {
                        view.attachments_busy = false;
                        match other {
                            Ok(Paste::Text(s)) => {
                                view.draft.perform(iced::widget::text_editor::Action::Edit(
                                    iced::widget::text_editor::Edit::Paste(Arc::new(s)),
                                ))
                            }
                            Err(why) => view.failed = Some(why),
                            _ => {}
                        }
                    }
                }
            },
            Event::Loaded(session, key, result) => {
                if let Some(view) = self.views.get_mut(&session) {
                    let used = view.decoded_media_bytes();
                    let result = result.and_then(|pic| {
                        if used + pic.rgba.len() > 128 * 1024 * 1024 {
                            Err("Session image cache is full (128 MiB).".into())
                        } else {
                            Ok(pic)
                        }
                    });
                    view.media.insert(key, Some(result));
                    view.heights = Default::default();
                }
                return self.load_media(&session);
            }
            Event::Reload(session, key) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.media.remove(&key);
                    view.media_requests.remove(&key);
                }
                return self.load_media(&session);
            }
            Event::Copy(session, key) => {
                let pic = self
                    .views
                    .get(&session)
                    .and_then(|v| v.media.get(&key))
                    .and_then(|p| p.as_ref())
                    .and_then(|p| p.as_ref().ok())
                    .cloned();
                if let Some(pic) = pic {
                    let token = self.views[&session].media_token.clone();
                    return crate::blocking(
                        move || {
                            clipboard(|c| {
                                c.set_image(arboard::ImageData {
                                    width: pic.width as usize,
                                    height: pic.height as usize,
                                    bytes: std::borrow::Cow::Owned(pic.rgba.as_ref().clone()),
                                })
                                .map_err(|e| e.to_string())
                            })
                        },
                        move |r| {
                            Message::Media(Event::Scoped(
                                session.clone(),
                                token.clone(),
                                Box::new(Event::Copied(session.clone(), r)),
                            ))
                        },
                    );
                }
            }
            Event::Copied(session, result) => {
                if let (Some(view), Err(why)) = (self.views.get_mut(&session), result) {
                    view.failed = Some(why);
                }
            }
        }
        Task::none()
    }
    pub(crate) fn attachment_paths(
        &self,
        session: String,
        paths: Option<Vec<std::path::PathBuf>>,
    ) -> Task<Message> {
        let Some(view) = self.views.get(&session) else {
            return Task::none();
        };
        let token = view.media_token.clone();
        let actions = self.actions.clone();
        let used = view.decoded_media_bytes();
        let budget = (128 * 1024 * 1024usize).saturating_sub(used);
        let dest = session.clone();
        crate::blocking(
            move || {
                let Some(paths) = paths else {
                    return Ok(vec![]);
                };
                if paths.len() > 64 {
                    return Err("Choose at most 64 attachments at a time.".into());
                }
                let mut total = 0u64;
                for path in &paths {
                    let meta = path
                        .metadata()
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    if !meta.is_file() {
                        return Err("Choose regular files.".into());
                    }
                    total = total.saturating_add(meta.len());
                    if total > LIMIT as u64 {
                        return Err("Selected files together exceed 10 MiB.".into());
                    }
                }
                let actions = actions.ok_or("No daemon connected")?;
                collect_attachments(
                    paths.into_iter().map(|p| attach(p, &session, &actions)),
                    budget,
                )
            },
            move |r| {
                Message::Media(Event::Scoped(
                    dest.clone(),
                    token.clone(),
                    Box::new(Event::Attached(dest.clone(), r)),
                ))
            },
        )
    }
    pub(crate) fn load_media(&mut self, session: &str) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        if view.media.len() >= 64 {
            return Task::none();
        }
        let home = view.files.home.as_ref().and_then(|p| p.to_path().ok());
        let mut keys: Vec<String> = view
            .conversation
            .items()
            .iter()
            .flat_map(|(_, i)| item_keys(i))
            .collect();
        for submission in view.conversation.submissions.values() {
            for a in &submission.attachments {
                if matches!(a, Attachment::Uploaded { mime, .. } if mime.starts_with("image/")) {
                    keys.push(key(a));
                }
            }
        }
        for rendered in view.rendered.values() {
            keys.extend(rendered.markdown.images().iter().map(|u| u.to_string()));
        }
        let mut tasks = Vec::new();
        let mut inflight = view.media.values().filter(|v| v.is_none()).count();
        for key in keys {
            if inflight >= 4 {
                break;
            }
            if view.media.contains_key(&key) || view.media.len() >= 64 {
                continue;
            }
            view.media.insert(key.clone(), None);
            let request = sill_client::request_id("image-read");
            view.media_requests.insert(key.clone(), request.clone());
            inflight += 1;
            let session = session.to_string();
            let dest = session.clone();
            let out = key.clone();
            let token = view.media_token.clone();
            let actions = actions.clone();
            let home = home.clone();
            tasks.push(crate::blocking(
                move || -> Result<Picture, String> {
                    let bytes = if let Some(id) = key.strip_prefix("upload:") {
                        let upload: Upload = actions
                            .read(
                                "_sill/uploads.get",
                                serde_json::json!({"session_id":session,"upload_id":id}),
                            )
                            .map_err(|e| format!("{e:#}"))?;
                        admin::decode_bytes(&upload.data_base64).map_err(|e| e.to_string())?
                    } else {
                        let path = key.strip_prefix("file://").unwrap_or(&key);
                        if path.contains("://") {
                            return Err("Remote image URLs are not fetched automatically.".into());
                        }
                        let decoded = percent_encoding::percent_decode_str(path)
                            .decode_utf8()
                            .map_err(|e| e.to_string())?;
                        let path = std::path::PathBuf::from(decoded.as_ref());
                        let path = if path.is_absolute() {
                            path
                        } else {
                            home.ok_or("Session home is not known; retry after opening Files.")?
                                .join(path)
                        };
                        let mut bytes = Vec::new();
                        let mut offset = "0".to_string();
                        let mut revision = None;
                        loop {
                            let page: admin::FileBytes = actions
                                .read(
                                    "_sill/files.image",
                                    serde_json::to_value(admin::SessionImage {
                                        session_id: session.clone(),
                                        path: sill_proto::sessions::HostPath::from_path(&path),
                                        offset: offset.clone(),
                                        length: Some(256 * 1024),
                                        revision,
                                    })
                                    .unwrap(),
                                )
                                .map_err(|e| format!("{e:#}"))?;
                            bytes.extend(
                                admin::decode_bytes(&page.bytes_base64)
                                    .map_err(|e| e.to_string())?,
                            );
                            if bytes.len() > 20 * 1024 * 1024 {
                                return Err("Image exceeds 20 MiB.".into());
                            }
                            if page.eof {
                                break;
                            }
                            if page.next_offset == offset {
                                return Err("Image read did not advance.".into());
                            }
                            offset = page.next_offset;
                            revision = Some(page.revision);
                        }
                        bytes
                    };
                    decode(&bytes)
                },
                move |r| {
                    Message::Media(Event::Scoped(
                        dest.clone(),
                        token.clone(),
                        Box::new(Event::Fetched(dest.clone(), out.clone(), request.clone(), r)),
                    ))
                },
            ));
        }
        Task::batch(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png() -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut output, ::image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }
    #[test]
    fn new_markdown_mention_refreshes_same_path_without_reloading_each_chunk() {
        let (mut app, _) = App::new(true, None);
        let view = app.views.get_mut("demo").unwrap();
        let mut item = view.conversation.item("agent").unwrap().clone();
        item.item_id = "image-result".into();
        item.content = vec![sill_proto::sfp::Content::text("![result](/tmp/same.png)")];
        view.media.insert("/tmp/same.png".into(), Some(decode(&png())));
        view.media_requests.insert("/tmp/same.png".into(), "old-read".into());
        view.conversation.insert_item(item.clone());
        view.render("image-result");
        assert!(!view.media.contains_key("/tmp/same.png"));
        assert!(!view.media_requests.contains_key("/tmp/same.png"));
        view.media.insert("/tmp/same.png".into(), Some(decode(&png())));
        item.content = vec![sill_proto::sfp::Content::text("![result](/tmp/same.png) More text")];
        view.conversation.insert_item(item.clone());
        view.render("image-result");
        assert!(view.media.contains_key("/tmp/same.png"));
        item.item_id = "next-result".into();
        view.conversation.insert_item(item);
        view.render("next-result");
        assert!(!view.media.contains_key("/tmp/same.png"));
        let _ = app.media_event(Event::Fetched("demo".into(), "/tmp/same.png".into(), "old-read".into(), decode(&png())));
        assert!(!app.views["demo"].media.contains_key("/tmp/same.png"));
    }

    #[test]
    fn expanded_image_closes_on_escape_and_restores_window_mode() {
        let (mut app, _) = App::new(true, None);
        app.views.get_mut("demo").unwrap().media.insert("test".into(), Some(decode(&png())));
        let _ = app.media_event(Event::Open("demo".into(), "test".into()));
        assert_eq!(app.views["demo"].image_viewer.as_deref(), Some("test"));
        let id = iced::window::Id::unique();
        let _ = app.media_event(Event::FullscreenReady("demo".into(), id, iced::window::Mode::Windowed));
        assert!(app.views["demo"].image_viewer_mode.is_some());
        let _ = app.update(Message::Escape);
        assert!(app.views["demo"].image_viewer.is_none());
        assert!(app.views["demo"].image_viewer_mode.is_none());
        let _ = app.media_event(Event::FullscreenReady("demo".into(), id, iced::window::Mode::Windowed));
        assert!(app.views["demo"].image_viewer_mode.is_none(), "late full-screen request after closing is ignored");
    }

    #[test]
    fn late_media_work_cannot_change_a_recreated_session_view() {
        let (mut app, _) = App::new(true, None);
        let token = app.views["demo"].media_token.clone();
        app.views
            .insert("demo".into(), crate::SessionView::new("demo"));
        app.views.get_mut("demo").unwrap().attachments_busy = true;
        let attachment = Attachment::Uploaded {
            upload_id: "old".into(),
            name: "old.txt".into(),
            mime: "text/plain".into(),
        };
        let events = [
            Event::Attached("demo".into(), Ok(vec![(attachment, None)])),
            Event::Pasted("demo".into(), Ok(Paste::Text("old clipboard".into()))),
            Event::Loaded("demo".into(), "old.png".into(), decode(&png())),
            Event::Copied("demo".into(), Err("old error".into())),
        ];
        for event in events {
            let _ = app.media_event(Event::Scoped("demo".into(), token.clone(), Box::new(event)));
        }
        let _ = app.update(Message::AttachmentPaths("demo".into(), token, None));
        let view = &app.views["demo"];
        assert!(
            view.attachments_busy,
            "an old completion cannot finish the new picker"
        );
        assert!(view.attachments.is_empty());
        assert!(view.draft.text().is_empty());
        assert!(view.media.is_empty());
        assert!(view.failed.is_none());
    }

    #[test]
    fn attachment_batch_stops_decoding_when_preview_budget_is_exhausted() {
        let mut decoded = 0;
        let files = (0..64).map(|i| {
            decoded += 1;
            make_image(format!("{i}.png"), png(), "image/png")
        });
        assert!(
            collect_attachments(files, 24)
                .unwrap_err()
                .contains("preview limit")
        );
        assert_eq!(
            decoded, 2,
            "remaining files must never be decoded or uploaded"
        );
    }

    #[test]
    fn png_preview_and_attachment_round_trip() {
        let bytes = png();
        let (a, p) = make_image("test.png".into(), bytes.clone(), "image/png").unwrap();
        let p = p.unwrap();
        assert_eq!((p.width, p.height, p.rgba.len()), (2, 3, 24));
        let Attachment::Inline { data_base64, .. } = a else {
            panic!()
        };
        assert_eq!(admin::decode_bytes(&data_base64).unwrap(), bytes);
        assert!(decode(b"not an image").is_err());
        assert!(make_image("huge.png".into(), vec![0; LIMIT + 1], "image/png").is_err());
    }
}

/// A full-window image viewer; the opaque layer prevents clicks reaching the conversation.
pub fn expanded<'a>(view: &crate::SessionView, key: &str) -> Element<'a, Message> {
    let content: Element<'a, Message> = match view.media.get(key) {
        Some(Some(Ok(pic))) => image(pic.handle.clone())
            .width(iced::Fill).height(iced::Fill)
            .content_fit(iced::ContentFit::Contain).into(),
        _ => text("Loading image…").into(),
    };
    iced::widget::opaque(container(column![
        iced::widget::row![
            text("Image").size(style::SMALL),
            iced::widget::space().width(iced::Fill),
            quiet_button("Copy image", Some(Message::Media(Event::Copy(view.id.clone(), key.into())))),
            quiet_button(if view.image_viewer_mode.is_some() { "Exit full screen" } else { "Full screen" },
                Some(Message::Media(Event::Fullscreen(view.id.clone())))),
            quiet_button("Close · Esc", Some(Message::Media(Event::Close))),
        ].spacing(12),
        content,
    ].spacing(12)).padding(16).width(iced::Fill).height(iced::Fill)
        .style(|_| container::Style { background: Some(style::P.code_bg.into()), ..Default::default() }))
}
