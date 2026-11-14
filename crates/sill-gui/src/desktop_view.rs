//! The live view of a session's test desktop (docs/permission-modes.md, Test desktop): the
//! desktop runs headless next to the jail, and this shows it in place of the conversation, the
//! composer staying below, from pictures the daemon takes outside the jail (`_sill/desktop.capture`
//! and `_sill/desktop.read`, capability `sill.desktop_view`). It refreshes about once a second
//! while shown; an unchanged desktop is not fetched again. View only: input is a later step.
use std::time::Instant;

use iced::widget::{button, center, column, container, image, row, text};
use iced::{Alignment, ContentFit, Element, Fill, Task};
use sill_proto::admin::{DesktopCapture, DesktopFrame, DesktopRead, FileBytes};
use sill_proto::sessions::Session;

use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, icons, keys};

pub const CAPABILITY: &str = "sill.desktop_view";
/// The largest picture fetched (as the daemon's own bound).
const MAX_PICTURE: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Event {
    /// Show or hide the view.
    Toggle(String),
    /// Time for a new picture of the shown view.
    Tick,
    /// A capture came back: a new picture, the same as shown (`None`), or why there is none.
    Captured(String, u64, Result<Option<Frame>, String>),
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub picture: crate::media::Picture,
    pub revision: String,
    /// The desktop's scale: its picture is this many pixels per point.
    pub scale: u32,
}

/// A session's live view in this window.
#[derive(Default)]
pub struct View {
    pub shown: bool,
    pub frame: Option<Frame>,
    capturing: Option<u64>,
    pub error: Option<String>,
    /// When the shown picture was taken.
    taken: Option<Instant>,
    /// Captures in a row that found the desktop unchanged: the next one waits longer (1 s, 2 s,
    /// then 3 s), and the first change brings it back to every second.
    unchanged: u32,
}

impl View {
    /// Whether a new capture is due on this tick.
    fn due(&self, now: Instant) -> bool {
        let wait = std::time::Duration::from_secs(u64::from(1 + self.unchanged.min(2)));
        self.taken.is_none_or(|taken| now.duration_since(taken) + std::time::Duration::from_millis(100) >= wait)
    }
}

/// The session's running jail has a test desktop (its launch says so).
pub fn available(view: &SessionView, session: Option<&Session>) -> bool {
    session.is_some_and(|s| s.running && s.ended_at.is_none())
        && view.permissions.views.as_ref().is_some_and(|v| {
            v.launch.as_ref().is_some_and(|launch| {
                launch
                    .permissions
                    .iter()
                    .any(|p| p.capability == "test_desktop" && p.effective)
            })
        })
}

impl App {
    pub(crate) fn desktop_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Toggle(id) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                view.desktop.shown = !view.desktop.shown;
                if view.desktop.shown {
                    view.desktop.error = None;
                    view.desktop.unchanged = 0;
                    return self.capture_desktop(&id);
                }
                // the conversation was out of the tree meanwhile: back to its place, not its top
                if self.selected.as_deref() == Some(id.as_str()) {
                    return self.restore_scroll();
                }
            }
            Event::Tick => {
                if let Some(id) = self.selected.clone()
                    && self.views.get(&id).is_some_and(|v| v.desktop.shown && v.desktop.due(Instant::now()))
                {
                    return self.capture_desktop(&id);
                }
            }
            Event::Captured(id, serial, result) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.desktop.capturing != Some(serial) {
                    return Task::none();
                }
                view.desktop.capturing = None;
                match result {
                    Ok(Some(frame)) => {
                        view.desktop.frame = Some(frame);
                        view.desktop.taken = Some(Instant::now());
                        view.desktop.error = None;
                        view.desktop.unchanged = 0;
                    }
                    Ok(None) => {
                        view.desktop.taken = Some(Instant::now());
                        view.desktop.error = None;
                        view.desktop.unchanged += 1;
                    }
                    Err(error) => view.desktop.error = Some(error),
                }
            }
        }
        Task::none()
    }

    /// A new picture, unless one is on its way.
    fn capture_desktop(&mut self, id: &str) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(id)) else {
            return Task::none();
        };
        if view.desktop.capturing.is_some() {
            return Task::none();
        }
        self.desktop_serial += 1;
        let serial = self.desktop_serial;
        view.desktop.capturing = Some(serial);
        let shown = view.desktop.frame.as_ref().map(|f| f.revision.clone());
        let (session, id) = (id.to_string(), id.to_string());
        blocking(
            move || {
                actions
                    .reading(|client| {
                        if !client.has_capability(CAPABILITY) {
                            anyhow::bail!(
                                "the daemon is older than this window and cannot show the test desktop ({CAPABILITY}); update and restart it"
                            );
                        }
                        let frame: DesktopFrame = client.call(
                            "_sill/desktop.capture",
                            DesktopCapture { session_id: session.clone(), since: shown.clone() },
                        )?;
                        if !frame.changed || shown.as_ref() == Some(&frame.revision) {
                            return Ok(None);
                        }
                        let size: usize = frame.size.parse()?;
                        if size > MAX_PICTURE {
                            anyhow::bail!("the picture exceeds {} MiB", MAX_PICTURE / 1024 / 1024);
                        }
                        let mut png = Vec::with_capacity(size);
                        loop {
                            let chunk: FileBytes = client.call(
                                "_sill/desktop.read",
                                DesktopRead {
                                    session_id: session.clone(),
                                    revision: frame.revision.clone(),
                                    offset: png.len().to_string(),
                                    length: None,
                                },
                            )?;
                            png.extend(sill_proto::admin::decode_bytes(&chunk.bytes_base64)?);
                            if png.len() > size {
                                anyhow::bail!("the picture is longer than announced");
                            }
                            if chunk.eof {
                                break;
                            }
                        }
                        let picture = crate::media::decode(&png).map_err(anyhow::Error::msg)?;
                        Ok(Some(Frame {
                            picture,
                            revision: frame.revision,
                            scale: frame.scale.max(1),
                        }))
                    })
                    .map_err(|e| said(&format!("{e:#}")))
            },
            move |result| Message::Desktop(Event::Captured(id, serial, result)),
        )
    }
}

/// A daemon refusal in words.
fn said(error: &str) -> String {
    if error.contains("no test desktop running") {
        "No test desktop is running in this session.".into()
    } else {
        format!("The test desktop could not be shown: {error}")
    }
}

/// The composer bar's monitor: shows or hides the live view, while the jail has a test desktop.
pub fn indicator<'a>(view: &'a SessionView, session: Option<&Session>) -> Option<Element<'a, Message>> {
    if !available(view, session) {
        return None;
    }
    let color = if view.desktop.shown { P.accent } else { P.fg_dim };
    let monitor = keys::press(
        crate::ui::bar_button(icons::glyph("monitor", crate::ui::BAR_ICON, color)).style(style::quiet),
        Message::Desktop(Event::Toggle(view.id.clone())),
    );
    Some(crate::ui::bar_tip(
        container(monitor).id("desktop-button"),
        if view.desktop.shown {
            "Hide the test desktop"
        } else {
            "Show the test desktop (view only)"
        },
    ))
}

/// The view in the conversation's place: a caption with the desktop's size and the close cross,
/// the picture fitted to the pane but never past its own size.
pub fn pane<'a>(view: &'a SessionView, session: Option<&Session>) -> Element<'a, Message> {
    let desktop = &view.desktop;
    let running = available(view, session);
    let status = match (&desktop.frame, &desktop.error, running) {
        (_, _, false) => "not running".to_string(),
        (_, Some(_), _) => "not updating".to_string(),
        (Some(frame), None, true) => format!(
            "{}×{} · view only",
            frame.picture.width / frame.scale,
            frame.picture.height / frame.scale
        ),
        (None, None, true) => "starting the view…".to_string(),
    };
    let close = keys::press(
        button(icons::glyph("x", 12.0, alpha(P.fg, 0.55)))
            .padding([4, 6])
            .style(style::quiet),
        Message::Desktop(Event::Toggle(view.id.clone())),
    );
    let caption = row![
        text("Test desktop").size(style::SMALL).color(P.fg),
        text(status).size(style::TINY).color(P.fg_dim).width(Fill),
        close,
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let body: Element<'a, Message> = match (&desktop.frame, running) {
        (Some(frame), true) => {
            let (w, h) = (
                (frame.picture.width / frame.scale) as f32,
                (frame.picture.height / frame.scale) as f32,
            );
            center(
                container(
                    image(frame.picture.handle.clone())
                        .content_fit(ContentFit::Contain)
                        .width(Fill)
                        .height(Fill),
                )
                .max_width(w)
                .max_height(h),
            )
            .into()
        }
        _ => center(
            text(match (&desktop.error, running) {
                (_, false) => "No test desktop is running in this session.".to_string(),
                (Some(error), _) => error.clone(),
                (None, _) => "Taking the first picture…".to_string(),
            })
            .size(style::SMALL)
            .color(P.fg_dim),
        )
        .into(),
    };
    let mut pane = column![container(caption).padding([4, 12])];
    if let (Some(error), Some(_)) = (&desktop.error, &desktop.frame) {
        pane = pane.push(container(text(error).size(style::TINY).color(P.error)).padding([0, 12]));
    }
    pane.push(container(body).padding(8).width(Fill).height(Fill).style(style::fill(P.bg)))
        .height(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{daemon, live_app, session_record, settle_task, simulator};
    use serde_json::json;

    /// A 4×2 RGB PNG, as the daemon sends: a 2×1 desktop at scale 2.
    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        ::image::RgbImage::from_pixel(4, 2, ::image::Rgb([200, 100, 50]))
            .write_to(&mut bytes, ::image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn with_desktop(app: &mut App, effective: bool) {
        let state = json!({"capability": "test_desktop", "allowed": true, "default_enabled": true,
            "requested": null, "blocked": false, "source": "policy", "effective": effective,
            "available": true, "reason": ""});
        let view: sill_proto::admin::PolicyView = serde_json::from_value(json!({"view": "launch",
            "session_id": "demo", "name": "default", "text": null, "fingerprint": null,
            "diagnostics": [], "permissions": [state], "session_hosts": []})).unwrap();
        app.views.get_mut("demo").unwrap().permissions.views =
            Some(crate::permissions::Views { preview: view.clone(), launch: Some(view), agent_name: None });
    }

    /// A client whose daemon answered the desktop view capability, and that daemon's end.
    fn viewing_client() -> (sill_proto::Client, crate::tests::Daemon) {
        let (client, mut daemon) = daemon();
        let initializing = std::thread::spawn(move || {
            client.initialize().unwrap();
            client
        });
        let request = daemon.expect("_sill/initialize");
        assert_eq!(request["params"]["capabilities"][CAPABILITY], 1, "offered by every client");
        let answer = sill_proto::initialize::Initialized {
            protocol: sill_proto::initialize::PROTOCOL_ID.into(),
            daemon: sill_proto::initialize::Peer { name: "sill".into(), version: "test".into() },
            capabilities: [(CAPABILITY.to_string(), 1)].into(),
            limits: Default::default(),
        };
        daemon.reply(&request, serde_json::to_value(answer).unwrap());
        (initializing.join().unwrap(), daemon)
    }

    /// Closing the view puts the conversation back where it was (the feed left the widget tree
    /// while the desktop showed, which would otherwise start it from the top).
    #[test]
    fn closing_the_view_restores_the_conversation_s_place() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        with_desktop(&mut app, true);
        {
            let view = app.views.get_mut("demo").unwrap();
            view.follow_tail = false;
            view.scroll_offset = 1234.0;
            view.desktop.shown = true;
        }
        let task = app.update(Message::Desktop(Event::Toggle("demo".into())));
        assert!(!app.views["demo"].desktop.shown);
        assert!(iced_runtime::task::into_stream(task).is_some(), "a scroll back to the saved offset");
    }

    /// The monitor is in the composer bar only while the running jail has a test desktop.
    #[test]
    fn the_monitor_shows_only_with_a_running_test_desktop() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let shown = |app: &App| simulator(app, (720.0, 860.0)).find(iced::widget::Id::new("desktop-button")).is_ok();
        assert!(!shown(&app));
        with_desktop(&mut app, true);
        assert!(shown(&app));
        with_desktop(&mut app, false);
        assert!(!shown(&app));
    }

    /// Showing the view captures the desktop and reads the picture; a picture with the same
    /// revision is not read again; the view says the desktop's own size.
    #[test]
    fn the_view_reads_new_pictures_and_skips_unchanged_ones() {
        let (client, mut daemon) = viewing_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        with_desktop(&mut app, true);
        let bytes = png();
        let size = bytes.len();
        let fake = std::thread::spawn(move || {
            let frame = json!({"revision": "r1", "changed": true, "width": 4, "height": 2, "scale": 2, "size": size.to_string()});
            let capture = daemon.expect("_sill/desktop.capture");
            assert_eq!(capture["params"], json!({"session_id": "demo", "since": null}));
            daemon.reply(&capture, frame.clone());
            let read = daemon.expect("_sill/desktop.read");
            assert_eq!(read["params"]["revision"], "r1");
            assert_eq!(read["params"]["offset"], "0");
            daemon.reply(&read, json!({"bytes_base64": sill_proto::admin::encode_bytes(&bytes),
                "next_offset": size.to_string(), "eof": true, "revision": "r1"}));
            // the same picture again: the window says which it has, no read follows
            let again = daemon.expect("_sill/desktop.capture");
            assert_eq!(again["params"]["since"], "r1");
            daemon.reply(&again, json!({"revision": "r1", "changed": false, "width": 4, "height": 2, "scale": 2, "size": "0"}));
        });
        let task = app.update(Message::Desktop(Event::Toggle("demo".into())));
        settle_task(&mut app, task);
        let shown = app.views["demo"].desktop.frame.as_ref().map(|f| (f.picture.width, f.scale));
        assert_eq!(shown, Some((4, 2)));
        simulator(&app, (720.0, 860.0)).find("2×1 · view only").unwrap();
        // a second later: the next capture is due
        app.views.get_mut("demo").unwrap().desktop.taken = Some(Instant::now() - std::time::Duration::from_secs(2));
        let task = app.update(Message::Desktop(Event::Tick));
        settle_task(&mut app, task);
        fake.join().unwrap();
        assert!(app.views["demo"].desktop.error.is_none());
    }

    /// An unchanged desktop is asked about less often, a change brings back every second.
    #[test]
    fn unchanged_pictures_slow_the_polling_down() {
        let now = Instant::now();
        let mut view = View::default();
        assert!(view.due(now), "the first picture at once");
        view.taken = Some(now);
        assert!(view.due(now + std::time::Duration::from_secs(1)));
        view.unchanged = 1;
        assert!(!view.due(now + std::time::Duration::from_secs(1)) && view.due(now + std::time::Duration::from_secs(2)));
        view.unchanged = 9;
        assert!(!view.due(now + std::time::Duration::from_secs(2)) && view.due(now + std::time::Duration::from_secs(3)));
    }

    /// A daemon without the capability is told apart here, without a request.
    #[test]
    fn an_older_daemon_is_told_apart() {
        let (client, _quiet) = daemon();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        with_desktop(&mut app, true);
        let task = app.update(Message::Desktop(Event::Toggle("demo".into())));
        settle_task(&mut app, task);
        let error = app.views["demo"].desktop.error.clone().expect("refused here");
        assert!(error.contains(CAPABILITY), "{error}");
    }
}
