//! App-wide daemon observations, independent of any selected session.
use crate::{App, Message, blocking};
use iced::widget::{button, column, row, text};
use iced::{Element, Task};

#[derive(Debug, Clone)]
pub enum Event {
    Check,
    Open,
    Restart,
    Checked(u64, Result<sill_proto::DaemonStatus, (bool, String)>),
    Restarted(u64, Result<(), String>),
}
#[derive(Default)]
pub struct State {
    pub status: Option<sill_proto::DaemonStatus>,
    pub problem: Option<String>,
    pub restart_error: Option<String>,
    pub stopped: bool,
    pub pending: bool,
    pub restarting: bool,
    serial: u64,
}
impl State {
    pub fn trouble(&self) -> bool {
        self.status.is_none() || self.problem.is_some() || self.restarting
    }
    pub fn label(&self) -> String {
        if self.restarting {
            "Daemon restarting…".into()
        } else if let Some(error) = &self.problem {
            error.clone()
        } else if self.status.is_some() {
            "Daemon responding".into()
        } else {
            "Checking daemon…".into()
        }
    }
}
impl App {
    pub(crate) fn daemon_trouble(&self) -> bool {
        self.daemon.trouble() || self.list_error.is_some()
    }
    pub(crate) fn daemon_label(&self) -> String {
        if !self.daemon.trouble()
            && let Some(error) = &self.list_error
        {
            format!("Daemon responding; session list unavailable: {error}")
        } else {
            self.daemon.label()
        }
    }
    pub(crate) fn daemon_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Open => {
                let task = if self.settings_page.is_none() {
                    self.show_page(crate::Page::Settings)
                } else {
                    Task::none()
                };
                if let Some(page) = &mut self.settings_page {
                    page.category = crate::settings_ui::Category::System;
                    page.collapsed.remove("Daemon");
                    self.settings.settings_category = page.category;
                    // only the person's own window writes their file: never the demo or a test
                    if self.persist {
                        crate::settings::save(&self.settings);
                    }
                }
                Task::batch([
                    task,
                    iced::widget::operation::snap_to(
                        "settings",
                        iced::widget::scrollable::RelativeOffset::START,
                    ),
                ])
            }
            Event::Check => {
                if self.daemon.pending || self.daemon.restarting || self.demo {
                    return Task::none();
                }
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                self.daemon.pending = true;
                self.daemon.serial += 1;
                let serial = self.daemon.serial;
                blocking(
                    move || {
                        let result = connect().and_then(|client| {
                            client.call_within(
                                sill_proto::method::DAEMON_STATUS,
                                sill_proto::Empty {},
                                sill_proto::client::QUICK_TIMEOUT,
                            )
                        });
                        result.map_err(|e| {
                            let local = matches!(sill_proto::transport::selected(), Ok(None));
                            let stopped = local
                                && e.chain()
                                    .filter_map(|e| e.downcast_ref::<std::io::Error>())
                                    .any(|e| {
                                        matches!(
                                            e.kind(),
                                            std::io::ErrorKind::NotFound
                                                | std::io::ErrorKind::ConnectionRefused
                                        )
                                    });
                            (
                                stopped,
                                format!(
                                    "Daemon {}: {e:#}",
                                    if stopped { "stopped" } else { "unreachable" }
                                ),
                            )
                        })
                    },
                    move |result| Message::Daemon(Event::Checked(serial, result)),
                )
            }
            Event::Checked(serial, result) => {
                if serial != self.daemon.serial {
                    return Task::none();
                }
                self.daemon.pending = false;
                match result {
                    Ok(status) => {
                        self.daemon.status = Some(status);
                        self.daemon.problem = None;
                        self.daemon.stopped = false;
                    }
                    Err((stopped, error)) => {
                        self.daemon.problem = Some(error);
                        self.daemon.stopped = stopped;
                    }
                }
                Task::none()
            }
            Event::Restart => {
                if self.daemon.restarting || self.demo {
                    return Task::none();
                }
                self.daemon.serial += 1;
                let serial = self.daemon.serial;
                self.daemon.pending = false;
                self.daemon.restarting = true;
                self.daemon.restart_error = None;
                blocking(restart, move |result| {
                    Message::Daemon(Event::Restarted(serial, result))
                })
            }
            Event::Restarted(serial, result) => {
                if serial != self.daemon.serial {
                    return Task::none();
                }
                self.daemon.restarting = false;
                if let Err(error) = result {
                    self.daemon.restart_error = Some(error.clone());
                    self.daemon.problem = Some(error);
                    return Task::none();
                }
                self.daemon_event(Event::Check)
            }
        }
    }
    pub(crate) fn daemon_rows<'a>(
        &'a self,
        page: &crate::settings_ui::SettingsPage,
    ) -> Vec<Element<'a, Message>> {
        use crate::settings_ui::{Event as SettingsEvent, quiet_button};
        use crate::style::{self, P};
        use iced::widget::{container, rule, space};
        use iced::{Alignment, Fill};
        let (label, color) = if self.demo {
            ("Demo", P.fg_dim)
        } else if self.daemon.restarting {
            ("Restarting…", P.warning)
        } else if self.daemon.stopped {
            ("Stopped", P.error)
        } else if self.daemon.problem.is_some() {
            ("Unreachable", P.warning)
        } else if self.daemon.status.is_none() {
            ("Checking…", P.warning)
        } else if self.list_error.is_some() {
            ("Connection issue", P.warning)
        } else {
            ("Connected", P.success)
        };
        let dot = container(space().width(6).height(6)).style(move |_| container::Style {
            background: Some(color.into()),
            border: iced::Border {
                radius: 3.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut card = column![
            row![
                text("Daemon").size(style::TEXT).width(Fill),
                text(label).size(style::TEXT).color(color),
                dot,
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        ]
        .spacing(12);
        if self.demo {
            card = card.push(
                text("No daemon is connected in demo mode.")
                    .size(style::SMALL)
                    .color(P.fg_dim),
            );
        } else if self.daemon_trouble()
            && !self.daemon.restarting
            && self.daemon.restart_error.is_none()
        {
            card = card.push(text(self.daemon_label()).size(style::SMALL).color(P.fg_dim));
        }
        let summary = |s: &sill_proto::DaemonStatus| {
            column![
                detail_row("Running since", readable_started(&s.started)),
                detail_row(
                    "Sessions",
                    format!("{} running · {} total", s.running, s.sessions)
                ),
            ]
            .spacing(8)
        };
        if !self.daemon_trouble()
            && !self.demo
            && let Some(s) = &self.daemon.status
        {
            card = card.push(summary(s));
        }
        let local = matches!(sill_proto::transport::selected(), Ok(None));
        let autostart_control = crate::settings_ui::switch_styled(
            page.autostart.as_ref().copied().unwrap_or(false),
            P.accent,
            20.0,
            (local && !self.demo && !page.autostart_busy && page.autostart.is_ok())
                .then_some(|enabled| Message::Settings(SettingsEvent::Autostart(enabled))),
        );
        card = card.push(
            row![text("Daemon autostart").size(style::TEXT).width(Fill), autostart_control]
                .spacing(16)
                .align_y(Alignment::Center),
        ).push(text(if !local {
            "Autostart is a local account setting. Change it on the remote box."
        } else if page.autostart_busy {
            "Updating autostart…"
        } else {
            "Start at login and when opening Sill. Turning off may stop the daemon; sessions keep running. Restart daemon starts it manually."
        }).size(style::SMALL).color(P.fg_dim));
        if local && let Err(why) = &page.autostart {
            card = card.push(text(why.clone()).size(style::SMALL).color(P.error));
        }
        let expanded = !page.collapsed.contains("Daemon technical details");
        card = card.push(rule::horizontal(1).style(style::rule_style(P.border)));
        card = card.push(crate::keys::press(
            button(
                text(if expanded {
                    "▾ Technical details"
                } else {
                    "▸ Technical details"
                })
                .size(style::SMALL),
            )
            .padding([4, 0])
            .style(style::quiet),
            Message::Settings(SettingsEvent::ToggleSection("Daemon technical details")),
        ));
        if expanded {
            let mut details = column![
                detail_row(
                    "Connection",
                    sill_proto::transport::selected_name().unwrap_or_else(|| "Local".into())
                ),
                detail_row("GUI version", env!("CARGO_PKG_VERSION").into()),
            ]
            .spacing(8);
            if let Some(s) = &self.daemon.status {
                if self.daemon_trouble() {
                    details = details
                        .push(
                            text("Last known daemon information")
                                .size(style::SMALL)
                                .color(P.fg_dim),
                        )
                        .push(summary(s));
                }
                details = details
                    .push(detail_row("Daemon version", s.version.clone()))
                    .push(detail_row("Protocol", s.protocol.clone()))
                    .push(detail_row("Process ID", s.pid.to_string()))
                    .push(detail_row(
                        "Service mode",
                        match s.mode.as_str() {
                            "unit" => "System service".into(),
                            "direct" => "Direct".into(),
                            _ => s.mode.clone(),
                        },
                    ))
                    .push(detail_row(
                        "Resource limits",
                        if s.limits { "Available" } else { "Unavailable" }.into(),
                    ))
                    .push(detail_row(
                        "Session logs",
                        if s.journal { "System journal" } else { "Files" }.into(),
                    ))
                    .push(detail_row("Socket", s.socket.display.clone()));
            }
            card = card.push(details);
        }
        let enabled = !self.demo && !self.daemon.restarting;
        card = card.push(
            row![
                quiet_button(
                    if self.daemon.pending {
                        "Checking…"
                    } else {
                        "Check status"
                    },
                    (enabled && !self.daemon.pending).then_some(Message::Daemon(Event::Check))
                ),
                quiet_button(
                    "Restart daemon",
                    enabled.then_some(Message::Daemon(Event::Restart))
                ),
            ]
            .spacing(8),
        );
        if let Some(error) = &self.daemon.restart_error {
            card = card.push(text(error).size(style::SMALL).color(P.error));
        } else if self.daemon.restarting {
            card = card.push(
                text("Restarting the daemon and waiting for it to reconnect…")
                    .size(style::SMALL)
                    .color(P.fg_dim),
            );
        } else {
            card = card.push(
                text("Restart briefly disconnects this window. Sessions keep running.")
                    .size(style::TINY)
                    .color(P.fg_dim),
            );
        }
        vec![container(card).padding(16).into()]
    }
}

pub(crate) fn detail_row(label: &'static str, value: String) -> Element<'static, Message> {
    use crate::style::{self, P};
    use iced::{Alignment, FillPortion};
    row![
        text(label)
            .size(style::SMALL)
            .color(P.fg_dim)
            .width(FillPortion(2)),
        text(value)
            .size(style::SMALL)
            .width(FillPortion(3))
            .align_x(Alignment::End)
    ]
    .spacing(16)
    .into()
}

fn readable_started(started: &str) -> String {
    let readable = started.replace('T', "  ");
    if let Some(utc) = readable.strip_suffix('Z') {
        format!("{utc} UTC")
    } else {
        readable
    }
}

pub(crate) fn set_autostart(enabled: bool) -> Result<(), String> {
    daemon_command(&["daemon", "autostart", if enabled { "on" } else { "off" }])
}

fn restart() -> Result<(), String> {
    daemon_command(&["daemon", "restart"])
}

fn daemon_command(args: &[&str]) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let sibling = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("sill"))
        .filter(|p| p.is_file());
    let binary = sibling.unwrap_or_else(|| "sill".into());
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start sill command: {e}"))?;
    let stderr = child.stderr.take().expect("piped restart stderr");
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut message = String::new();
        let _ = stderr.take(16 * 1024).read_to_string(&mut message);
        let _ = sent.send(message);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    let detail = received
                        .recv_timeout(std::time::Duration::from_millis(100))
                        .unwrap_or_default();
                    Err(format!(
                        "Sill command failed ({status}): {}",
                        detail.trim()
                    ))
                };
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "Command timed out; its outcome is unknown. Check status before retrying.".into(),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{connector, daemon, settle_task};
    fn status() -> sill_proto::DaemonStatus {
        serde_json::from_value(serde_json::json!({"version":"test", "protocol":"sfp/1", "pid":123,
            "started":"2026-11-06T12:00:00Z", "mode":"direct", "launcher":"direct", "limits":true,
            "journal":false, "socket":{"display":"/tmp/daemon.sock", "bytes_base64":"L3RtcC9kYWVtb24uc29jaw=="}, "sessions":2,"running":1})).unwrap()
    }
    #[test]
    #[ignore = "writes daemon settings PNGs for review"]
    fn render_daemon_settings() {
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        app.daemon.status = Some(status());
        app.daemon.problem = Some("Daemon stopped: local socket refused the connection".into());
        app.daemon.stopped = true;
        let mut page = crate::settings_ui::SettingsPage::default();
        page.snapshot = Some(Err("Settings are unavailable while disconnected.".into()));
        app.settings_page = Some(page);
        crate::snapshots::render(&app, "daemon-card-offline.png");
        crate::snapshots::render_size(&app, "daemon-card-narrow.png", (420.0, 680.0));
        app.settings_page
            .as_mut()
            .unwrap()
            .collapsed
            .remove("Daemon technical details");
        crate::snapshots::render_size(&app, "daemon-card-details.png", (420.0, 680.0));
        app.daemon.problem = None;
        app.daemon.stopped = false;
        app.settings_page
            .as_mut()
            .unwrap()
            .collapsed
            .insert("Daemon technical details");
        app.settings_page.as_mut().unwrap().snapshot = None;
        crate::snapshots::render_size(&app, "daemon-card-connected.png", (420.0, 680.0));
    }
    #[test]
    fn checks_are_bounded_and_recover_after_failure() {
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        let (client, mut peer) = daemon();
        app.connect = Some(connector(vec![client]));
        let server = std::thread::spawn(move || {
            let request = peer.expect(sill_proto::method::DAEMON_STATUS);
            peer.reply(&request, serde_json::to_value(status()).unwrap());
        });
        let task = app.daemon_event(Event::Check);
        let serial = app.daemon.serial;
        let _ = app.daemon_event(Event::Check);
        assert_eq!(app.daemon.serial, serial);
        settle_task(&mut app, task);
        server.join().unwrap();
        assert!(!app.daemon.trouble());
        let _ = app.daemon_event(Event::Checked(serial, Err((true, "stopped".into()))));
        assert!(app.daemon.stopped);
        assert!(app.daemon.trouble());
        assert!(app.daemon.status.is_some()); // keep last-known details
        let _ = app.daemon_event(Event::Checked(serial, Ok(status())));
        assert!(!app.daemon.trouble());
        assert!(!app.daemon.stopped);
    }
    #[test]
    fn stale_checks_cannot_overwrite_restart_and_settings_remain_accessible() {
        let (mut app, _) = App::new(true, None);
        app.daemon.serial = 2;
        app.daemon.restarting = true;
        let _ = app.daemon_event(Event::Checked(1, Ok(status())));
        assert!(app.daemon.status.is_none());
        assert!(app.daemon.restarting);
        let _ = app.daemon_event(Event::Open);
        assert!(app.settings_page.is_some());
        assert!(
            !app.settings_page
                .as_ref()
                .unwrap()
                .collapsed
                .contains("Daemon")
        );
        let _ = app.daemon_event(Event::Restarted(1, Ok(())));
        assert!(app.daemon.restarting);
        let _ = app.daemon_event(Event::Restarted(2, Err("failed".into())));
        assert!(!app.daemon.restarting);
        assert_eq!(app.daemon.label(), "failed");
    }
}
