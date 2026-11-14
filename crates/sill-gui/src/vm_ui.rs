//! The Settings page's Virtual machine section: the Mac window's own VM (`local_vm.rs`, docs/sill-stick.md
//! section 9) — how far it is, what it is made of, the Mac folders it shares, and Restart, Shut down
//! and Start. Shown on the Mac when the window uses its own VM, and with examples in the demo.
//!
//! It is drawn apart from the daemon's settings, which come from the daemon in the VM: when the VM
//! is down or failed, this section is still there to do something about it.
use std::path::PathBuf;

use iced::widget::{column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Fill, Task};
use serde::{Deserialize, Serialize};

use crate::settings_ui::{quiet_button, setting, wide};
use crate::style::{self, P, alpha};
use crate::{App, Message, blocking};

/// How far the VM is, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Phase {
    Checking,
    /// Making the data disks and the key: the first start on this Mac.
    SettingUp,
    /// A newer system image was put in place.
    Updating,
    Starting,
    Ready,
    /// Shut down from Settings, the guest asked first.
    Stopping,
    Stopped,
    /// In plain words, for the person; what to do about it included.
    Failed(String),
}

impl Phase {
    /// On its way somewhere: the section looks again until it gets there.
    pub fn moving(&self) -> bool {
        matches!(
            self,
            Phase::Checking
                | Phase::SettingUp
                | Phase::Updating
                | Phase::Starting
                | Phase::Stopping
        )
    }
}

/// The VM's processors and memory as chosen in Settings (`resources.json` in the VM's directory);
/// `None` is sized for this Mac (`sill_vm::Config::sized_for_this_mac`). They apply at a start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gb: Option<u64>,
}

/// What the section shows of the VM, read off the window's thread.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// The VM's directory on this Mac.
    pub dir: PathBuf,
    /// The system image's build (its commit), as placed.
    pub image: Option<String>,
    /// Space taken on the Mac: the system disk, and the data disks (home, state) of `data_size` each.
    pub system_used: Option<u64>,
    pub home_used: Option<u64>,
    pub state_used: Option<u64>,
    pub data_size: Option<u64>,
    /// What the running VM was started with: processors and GB of memory.
    pub running: Option<(usize, u64)>,
    pub chosen: Resources,
    /// What Automatic means on this Mac, and the most that can be chosen.
    pub automatic: (usize, u64),
    pub max: (usize, u64),
    /// The shared Mac folders by share slot; `false` when the folder is gone from the Mac.
    pub folders: Vec<(usize, PathBuf, bool)>,
    pub slots: usize,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Read the VM's info again.
    Read,
    ReadSpace,
    Space(Result<sill_proto::storage::Space, String>),
    ScanCaches,
    Caches(Result<sill_proto::storage::Caches, String>),
    ClearCache(String),
    CancelClear,
    Cleared(Result<(), String>),
    OpenStorage,
    Info(Option<Box<Info>>),
    /// Once a second while the VM is on its way somewhere.
    Tick,
    Restart,
    Stop,
    Start,
    Resources(Resources),
    AddFolder,
    Picked(Option<PathBuf>),
    /// Unshare a folder: the first press asks, the second does it.
    RemoveFolder(PathBuf),
    Reveal,
    /// An action done: what to say of it, or why it did not happen.
    Done(Result<String, String>),
}

#[derive(Default)]
pub struct State {
    pub info: Option<Info>,
    pub space: Option<Result<sill_proto::storage::Space, String>>,
    caches: Option<Result<sill_proto::storage::Caches, String>>,
    space_reading: bool,
    scanning: bool,
    clearing: Option<String>,
    armed_clear: Option<String>,
    space_read_at: Option<std::time::Instant>,
    /// What is under way ("Restarting…"); the buttons wait for it.
    busy: Option<&'static str>,
    said: Option<Result<String, String>>,
    armed_remove: Option<PathBuf>,
    reading: bool,
    /// The phase last seen by Tick, to read the info again when it changes.
    seen: Option<Phase>,
}

fn msg(event: Event) -> Message {
    Message::Vm(event)
}

// ---- Where the work is done: the Mac's own VM, or nothing elsewhere (the demo shows examples).

#[cfg(target_os = "macos")]
use crate::local_vm as backend;

#[cfg(not(target_os = "macos"))]
mod backend {
    use super::{Info, Phase, Resources};
    const NONE: &str = "This window has no VM of its own.";
    pub fn phase() -> Option<Phase> {
        None
    }
    pub fn info() -> Option<Info> {
        None
    }
    pub fn restart() -> Result<(), String> {
        Err(NONE.into())
    }
    pub fn stop() -> Result<(), String> {
        Err(NONE.into())
    }
    pub fn start() -> Result<(), String> {
        Err(NONE.into())
    }
    pub fn set_resources(_: Resources) -> Result<(), String> {
        Err(NONE.into())
    }
    pub fn expose(_: &std::path::Path) -> Result<std::path::PathBuf, String> {
        Err(NONE.into())
    }
    pub fn unshare(_: &std::path::Path) -> Result<(), String> {
        Err(NONE.into())
    }
    pub fn reveal() -> Result<(), String> {
        Err(NONE.into())
    }
}

/// The demo's VM: examples, so the section can be seen without a Mac.
fn demo_info() -> Info {
    Info {
        dir: PathBuf::from("/Users/someone/Library/Application Support/Sill/vm"),
        image: Some("f2664859593f".into()),
        system_used: Some(1_950 << 20),
        home_used: Some(3_400 << 20),
        state_used: Some(820 << 20),
        data_size: Some(64 << 30),
        running: Some((4, 6)),
        chosen: Resources::default(),
        automatic: (4, 6),
        max: (10, 12),
        folders: vec![
            (0, PathBuf::from("/Users/someone/GIT/sill"), true),
            (1, PathBuf::from("/Users/someone/Documents/notes"), true),
            (2, PathBuf::from("/Volumes/Backup/photos"), false),
        ],
        slots: 16,
    }
}

const DEMO: &str = "The demo has no VM: these are examples.";

impl App {
    /// The section is shown: the Mac window's own VM, or the demo's examples.
    pub(crate) fn vm_shown(&self) -> bool {
        self.demo || backend::phase().is_some()
    }

    fn vm_phase(&self) -> Option<Phase> {
        if self.demo {
            Some(Phase::Ready)
        } else {
            backend::phase()
        }
    }

    /// While Settings shows a VM on its way somewhere, or an action under way: look once a second.
    pub(crate) fn vm_ticking(&self) -> bool {
        self.settings_page.is_some()
            && !self.demo
            && (self.vm.busy.is_some()
                || self
                    .vm_phase()
                    .is_some_and(|p| p.moving() || p == Phase::Ready))
    }

    pub(crate) fn vm_event(&mut self, event: Event) -> Task<Message> {
        if !self.vm_shown() {
            return Task::none();
        }
        let act = |busy: &'static str,
                   state: &mut State,
                   work: fn() -> Result<String, String>|
         -> Task<Message> {
            if state.busy.is_some() {
                return Task::none();
            }
            state.busy = Some(busy);
            state.said = None;
            state.armed_remove = None;
            blocking(work, |result| msg(Event::Done(result)))
        };
        match event {
            Event::OpenStorage => {
                let task = if self.settings_page.is_none() {
                    self.show_page(crate::Page::Settings)
                } else {
                    Task::none()
                };
                if let Some(page) = &mut self.settings_page {
                    page.category = crate::settings_ui::Category::System;
                    page.collapsed.remove("Virtual machine");
                    page.collapsed.insert("Daemon");
                }
                return Task::batch([
                    task,
                    self.vm_event(Event::ReadSpace),
                    iced::widget::operation::snap_to(
                        "settings",
                        iced::widget::scrollable::RelativeOffset::START,
                    ),
                ]);
            }
            Event::ReadSpace => {
                if self.vm.space_reading {
                    return Task::none();
                }
                if self.demo {
                    self.vm.space = Some(Ok(sill_proto::storage::Space {
                        home: sill_proto::storage::DiskSpace {
                            total: 63 << 30,
                            used: 16 << 30,
                            available: 47 << 30,
                        },
                        state: sill_proto::storage::DiskSpace {
                            total: 63 << 30,
                            used: 46 << 30,
                            available: 17 << 30,
                        },
                    }));
                    return Task::none();
                }
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                self.vm.space_reading = true;
                self.vm.space_read_at = Some(std::time::Instant::now());
                return blocking(
                    move || {
                        let client = connect().map_err(|e| format!("{e:#}"))?;
                        if !client.has_capability("sill.storage") {
                            return Err(
                                "Update the VM's daemon to read free space and clear build caches."
                                    .into(),
                            );
                        }
                        client
                            .call_within(
                                "_sill/storage.space",
                                sill_proto::Empty {},
                                sill_proto::client::QUICK_TIMEOUT,
                            )
                            .map_err(|e| format!("{e:#}"))
                    },
                    |r| msg(Event::Space(r)),
                );
            }
            Event::Space(result) => {
                self.vm.space_reading = false;
                self.vm.space = Some(result);
            }
            Event::ScanCaches => {
                if self.vm.scanning || self.vm.clearing.is_some() {
                    return Task::none();
                }
                self.vm.armed_clear = None;
                if self.demo {
                    self.vm.caches = Some(Ok(sill_proto::storage::Caches {
                        caches: vec![sill_proto::storage::BuildCache {
                            session_id: "604fd5e1".into(),
                            title: "Earlier build".into(),
                            bytes: 7_500 << 20,
                            active: false,
                            path: sill_proto::sessions::HostPath::from_path(std::path::Path::new(
                                "/var/lib/sill/1000/sessions/604fd5e1/private/home%sill%.cache/sill-dev/target",
                            )),
                            problem: None,
                        }],
                    }));
                    return Task::none();
                }
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                self.vm.scanning = true;
                return blocking(
                    move || {
                        let client = connect().map_err(|e| format!("{e:#}"))?;
                        client
                            .call_within(
                                "_sill/storage.caches",
                                sill_proto::Empty {},
                                std::time::Duration::from_secs(120),
                            )
                            .map_err(|e| format!("{e:#}"))
                    },
                    |r| msg(Event::Caches(r)),
                );
            }
            Event::Caches(result) => {
                self.vm.scanning = false;
                self.vm.caches = Some(result);
            }
            Event::CancelClear => self.vm.armed_clear = None,
            Event::ClearCache(id) => {
                if self.vm.scanning || self.vm.clearing.is_some() {
                    return Task::none();
                }
                let eligible = self
                    .vm
                    .caches
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .is_some_and(|r| {
                        r.caches
                            .iter()
                            .any(|c| c.session_id == id && !c.active && c.problem.is_none())
                    });
                if !eligible {
                    return Task::none();
                }
                if self.vm.armed_clear.as_ref() != Some(&id) {
                    self.vm.armed_clear = Some(id);
                    return Task::none();
                }
                self.vm.armed_clear = None;
                if self.demo {
                    self.vm.said = Some(Err(DEMO.into()));
                    return Task::none();
                }
                let Some(connect) = self.connect.clone() else {
                    return Task::none();
                };
                self.vm.clearing = Some(id.clone());
                self.vm.said = None;
                return blocking(
                    move || {
                        let client = connect().map_err(|e| format!("{e:#}"))?;
                        client
                            .call_within::<serde_json::Value>(
                                "_sill/storage.clear_build_cache",
                                sill_proto::storage::ClearBuildCache { session_id: id },
                                std::time::Duration::from_secs(120),
                            )
                            .map(|_| ())
                            .map_err(|e| format!("{e:#}"))
                    },
                    |r| msg(Event::Cleared(r)),
                );
            }
            Event::Cleared(result) => {
                self.vm.clearing = None;
                self.vm.said = Some(result.map(|()| {
                    "Build cache cleared. Refresh the list before clearing another cache.".into()
                }));
                // Never retry a timed-out deletion. A fresh scan shows what remains.
                self.vm.caches = None;
                return self.vm_event(Event::ReadSpace);
            }
            Event::Read => {
                if self.demo {
                    self.vm.info = Some(demo_info());
                    return self.vm_event(Event::ReadSpace);
                }
                if self.vm.reading {
                    return Task::none();
                }
                self.vm.reading = true;
                return blocking(
                    || backend::info().map(Box::new),
                    |info| msg(Event::Info(info)),
                );
            }
            Event::Info(info) => {
                self.vm.reading = false;
                self.vm.info = info.map(|i| *i);
                return self.vm_event(Event::ReadSpace);
            }
            Event::Tick => {
                let phase = self.vm_phase();
                if phase != self.vm.seen {
                    self.vm.seen = phase;
                    return self.vm_event(Event::Read);
                }
                if phase == Some(Phase::Ready)
                    && self
                        .vm
                        .space_read_at
                        .is_none_or(|t| t.elapsed().as_secs() >= 15)
                {
                    return self.vm_event(Event::ReadSpace);
                }
            }
            Event::Restart | Event::Stop | Event::Start if self.demo => {
                self.vm.said = Some(Err(DEMO.into()));
            }
            Event::Restart => {
                return act("Restarting…", &mut self.vm, || {
                    backend::restart().map(|()| "The VM restarted; sessions reconnect.".into())
                });
            }
            Event::Stop => {
                return act("Shutting down…", &mut self.vm, || {
                    backend::stop().map(|()| {
                        "The VM is shut down: no session runs until it starts again.".into()
                    })
                });
            }
            Event::Start => {
                return act("Starting…", &mut self.vm, || {
                    backend::start().map(|()| "The VM started.".into())
                });
            }
            Event::Resources(chosen) => {
                if self.demo {
                    if let Some(info) = &mut self.vm.info {
                        info.chosen = chosen;
                    }
                    return Task::none();
                }
                self.vm.said = None;
                return blocking(
                    move || {
                        backend::set_resources(chosen).map(|()| {
                            "Saved: the VM gets it at its next start (Restart).".to_string()
                        })
                    },
                    |result| msg(Event::Done(result)),
                );
            }
            Event::AddFolder => {
                if self.vm.busy.is_some() {
                    return Task::none();
                }
                self.vm.busy = Some("Choosing a folder…");
                self.vm.said = None;
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Share a folder with the VM")
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_path_buf())
                    },
                    |path| msg(Event::Picked(path)),
                );
            }
            Event::Picked(None) => self.vm.busy = None,
            Event::Picked(Some(path)) => {
                if self.demo {
                    self.vm.busy = None;
                    self.vm.said = Some(Err(DEMO.into()));
                    return Task::none();
                }
                self.vm.busy = Some("Sharing…");
                return blocking(
                    move || {
                        backend::expose(&path)
                            .map(|path| format!("{} is shared with the VM.", path.display()))
                    },
                    |result| msg(Event::Done(result)),
                );
            }
            Event::RemoveFolder(path) => {
                // not while something else is under way: the press would be lost
                if self.vm.busy.is_some() || self.vm_phase().is_some_and(|p| p.moving()) {
                    return Task::none();
                }
                if self.vm.armed_remove.as_ref() != Some(&path) {
                    self.vm.armed_remove = Some(path);
                    return Task::none();
                }
                self.vm.armed_remove = None;
                if self.demo {
                    self.vm.said = Some(Err(DEMO.into()));
                    return Task::none();
                }
                self.vm.busy = Some("Unsharing…");
                self.vm.said = None;
                return blocking(
                    move || {
                        backend::unshare(&path)
                            .map(|()| format!("{} is no longer shared.", path.display()))
                    },
                    |result| msg(Event::Done(result)),
                );
            }
            Event::Reveal => {
                if self.demo {
                    self.vm.said = Some(Err(DEMO.into()));
                    return Task::none();
                }
                if let Err(why) = backend::reveal() {
                    self.vm.said = Some(Err(why));
                }
            }
            Event::Done(result) => {
                let restarted = matches!(self.vm.busy, Some("Restarting…" | "Starting…"));
                self.vm.busy = None;
                self.vm.said = Some(result);
                let read = self.vm_event(Event::Read);
                // the daemon in the VM is a new process now: look at it again
                if restarted {
                    return Task::batch([read, self.daemon_event(crate::daemon_ui::Event::Check)]);
                }
                return read;
            }
        }
        Task::none()
    }

    /// The section's rows, under its title on the Settings page.
    fn vm_storage(&self) -> Element<'_, Message> {
        let vm = &self.vm;
        let ready = self.demo || self.vm_phase() == Some(Phase::Ready);
        let mut body = column![].spacing(10);
        match &vm.space {
            Some(Ok(space)) if ready => {
                for (label, disk) in [("Home disk", &space.home), ("State disk", &space.state)] {
                    body = body.push(crate::daemon_ui::detail_row(label, space_label(disk)));
                    if low_space(disk) {
                        body = body.push(text(format!("{label} is low on space. Clear unused build caches or make room before starting more work."))
                            .size(style::SMALL).color(P.warning));
                    }
                }
            }
            Some(Err(why)) => body = body.push(text(why).size(style::SMALL).color(P.warning)),
            _ => {
                body = body.push(
                    text(if ready {
                        "Reading free space…"
                    } else {
                        "Start the VM to read its free space."
                    })
                    .size(style::SMALL)
                    .color(P.fg_dim),
                )
            }
        }
        let supported = vm.space.as_ref().is_some_and(|r| r.is_ok()) && ready;
        body = body.push(
            row![
                quiet_button(
                    if vm.space_reading {
                        "Refreshing…"
                    } else {
                        "Refresh space"
                    },
                    (ready && !vm.space_reading).then(|| msg(Event::ReadSpace))
                ),
                quiet_button(
                    if vm.scanning {
                        "Scanning…"
                    } else {
                        "Scan build caches"
                    },
                    (supported && !vm.scanning && vm.clearing.is_none())
                        .then(|| msg(Event::ScanCaches))
                ),
            ]
            .spacing(8),
        );
        body = body.push(text("Only private ~/.cache/sill-dev/target build outputs are cleared. Source files, unapplied changes and toolchains are kept. Running sessions are excluded. An interrupted cleanup is included in the next scan.")
            .size(style::SMALL).color(P.fg_dim));
        match &vm.caches {
            Some(Ok(report)) => {
                if report.caches.is_empty() {
                    body = body.push(
                        text("No build caches found.")
                            .size(style::SMALL)
                            .color(P.fg_dim),
                    );
                }
                for cache in &report.caches {
                    let id = &cache.session_id;
                    let armed = vm.armed_clear.as_ref() == Some(id);
                    let clearing = vm.clearing.as_ref() == Some(id);
                    let title = if cache.title.is_empty() {
                        id.clone()
                    } else {
                        format!("{} · {id}", cache.title)
                    };
                    let mut line = column![
                        row![
                            text(title).size(style::SMALL),
                            space().width(Fill),
                            text(readable_size(cache.bytes)).size(style::SMALL)
                        ]
                        .spacing(8),
                        text(cache.path.display.clone())
                            .size(style::TINY)
                            .color(P.fg_dim),
                    ]
                    .spacing(4);
                    if let Some(problem) = &cache.problem {
                        line = line.push(text(problem).size(style::SMALL).color(P.warning));
                    }
                    let mut actions = row![quiet_button(
                        if cache.active {
                            "Running"
                        } else if clearing {
                            "Clearing…"
                        } else if armed {
                            "Confirm clear build cache"
                        } else {
                            "Clear build cache"
                        },
                        (supported
                            && !cache.active
                            && cache.problem.is_none()
                            && !vm.scanning
                            && vm.clearing.is_none())
                        .then(|| msg(Event::ClearCache(id.clone()))),
                    )]
                    .spacing(8);
                    if armed {
                        actions =
                            actions.push(quiet_button("Cancel", Some(msg(Event::CancelClear))));
                    }
                    body = body.push(line.push(actions));
                }
            }
            Some(Err(why)) => body = body.push(text(why).size(style::SMALL).color(P.warning)),
            None => (),
        }
        body.into()
    }

    pub(crate) fn vm_rows(&self) -> Vec<Element<'_, Message>> {
        let vm = &self.vm;
        let phase = self.vm_phase().unwrap_or(Phase::Checking);
        let info = vm.info.as_ref();
        let mut rows = Vec::new();

        // ---- Status and actions
        let acting = vm
            .busy
            .filter(|b| matches!(*b, "Restarting…" | "Shutting down…" | "Starting…"));
        let (label, color) = match &phase {
            _ if acting.is_some() => (acting.unwrap_or_default(), P.warning),
            Phase::Ready => ("Running", P.success),
            Phase::Checking | Phase::Starting => ("Starting…", P.warning),
            Phase::SettingUp => ("Setting up…", P.warning),
            Phase::Updating => ("Updating…", P.warning),
            Phase::Stopping => ("Shutting down…", P.warning),
            Phase::Stopped => ("Shut down", P.fg_dim),
            Phase::Failed(_) => ("Failed", P.error),
        };
        let dot = container(space().width(6).height(6)).style(move |_| container::Style {
            background: Some(color.into()),
            border: iced::Border {
                radius: 3.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut status = column![
            row![
                text("Linux VM").size(style::TEXT).width(Fill),
                text(label).size(style::TEXT).color(color),
                dot,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text("The daemon and every session run in it; it starts with Sill and ends when Sill quits.")
                .size(style::SMALL)
                .color(P.fg_dim),
        ]
        .spacing(10);
        if let Phase::Failed(said) = &phase {
            status = status.push(text(said.clone()).size(style::SMALL).color(P.error));
        }
        if let Some((cpus, memory)) = info.and_then(|i| i.running)
            && phase == Phase::Ready
        {
            status = status.push(crate::daemon_ui::detail_row(
                "Running with",
                format!("{cpus} processors · {memory} GB memory"),
            ));
        }
        let idle = vm.busy.is_none() && !phase.moving();
        let can_stop = idle && matches!(phase, Phase::Ready | Phase::Failed(_));
        let can_start = idle && matches!(phase, Phase::Stopped | Phase::Failed(_));
        let mut buttons = row![
            quiet_button("Restart", can_stop.then(|| msg(Event::Restart))),
            quiet_button(
                "Shut down",
                (can_stop && phase == Phase::Ready).then(|| msg(Event::Stop))
            ),
        ]
        .spacing(8);
        if matches!(phase, Phase::Stopped | Phase::Failed(_)) {
            buttons = buttons.push(quiet_button("Start", can_start.then(|| msg(Event::Start))));
        }
        buttons = buttons
            .push(space().width(Fill))
            .push(quiet_button("Show in Finder", Some(msg(Event::Reveal))));
        status = status.push(buttons);
        match &vm.said {
            Some(Ok(said)) => {
                status = status.push(text(said.clone()).size(style::SMALL).color(P.fg_dim));
            }
            Some(Err(why)) => {
                status = status.push(text(why.clone()).size(style::SMALL).color(P.error));
            }
            None => {}
        }
        rows.push(container(status).padding([12, 16]).into());
        rows.push(wide("Storage inside the VM", Some("Available space inside the VM is separate from the disk images' size on the Mac. Refreshes every 15 seconds while Settings is open."), self.vm_storage()));

        let Some(info) = info else {
            rows.push(
                container(text("Reading the VM…").size(style::SMALL).color(P.fg_dim))
                    .padding([12, 16])
                    .into(),
            );
            return rows;
        };

        // ---- Resources: apply at the next start
        // each row says whether its own choice differs from what the running VM has
        let running = info.running.filter(|_| phase == Phase::Ready);
        let note = |differs: bool| {
            if differs {
                "Changed: Restart to apply"
            } else {
                "Applies when the VM starts"
            }
        };
        let cpus_note = note(
            running.is_some_and(|(cpus, _)| info.chosen.cpus.unwrap_or(info.automatic.0) != cpus),
        );
        let memory_note = note(running.is_some_and(|(_, memory)| {
            info.chosen.memory_gb.unwrap_or(info.automatic.1) != memory
        }));
        let chosen = info.chosen;
        let cpu_options: Vec<Choice> = std::iter::once(Choice::automatic(format!(
            "Automatic ({})",
            info.automatic.0
        )))
        .chain((2..=info.max.0.max(2)).map(|n| Choice::value(n as u64, format!("{n}"))))
        .collect();
        let memory_options: Vec<Choice> = std::iter::once(Choice::automatic(format!(
            "Automatic ({} GB)",
            info.automatic.1
        )))
        .chain(
            MEMORY_GB
                .iter()
                .filter(|gb| **gb <= info.max.1.max(2))
                .map(|gb| Choice::value(*gb, format!("{gb} GB"))),
        )
        .collect();
        rows.push(setting(
            "Processors",
            Some(cpus_note),
            choose(cpu_options, chosen.cpus.map(|n| n as u64), move |v| {
                msg(Event::Resources(Resources {
                    cpus: v.map(|n| n as usize),
                    ..chosen
                }))
            }),
        ));
        rows.push(setting(
            "Memory",
            Some(memory_note),
            choose(memory_options, chosen.memory_gb, move |v| {
                msg(Event::Resources(Resources {
                    memory_gb: v,
                    ..chosen
                }))
            }),
        ));

        // ---- Shared folders
        let mut folders = column![].spacing(4);
        for (_, path, present) in &info.folders {
            let armed = vm.armed_remove.as_ref() == Some(path);
            let mut line = column![text(home_relative(path)).size(style::TEXT)].width(Fill);
            if !present {
                line = line.push(
                    text("Not on this Mac now (a disk not connected?)")
                        .size(style::SMALL)
                        .color(P.warning),
                );
            }
            let remove: Element<'_, Message> = if armed {
                quiet_button(
                    "Unshare?",
                    idle.then(|| msg(Event::RemoveFolder(path.clone()))),
                )
            } else {
                // waits while the VM restarts or another change is under way
                crate::keys::press_maybe(
                    iced::widget::button(crate::icons::glyph(
                        "minus",
                        13.0,
                        alpha(P.fg, if idle { 0.7 } else { 0.25 }),
                    ))
                    .padding([6, 8])
                    .style(style::quiet),
                    idle.then(|| msg(Event::RemoveFolder(path.clone()))),
                )
            };
            folders = folders.push(row![line, remove].spacing(8).align_y(Alignment::Center));
        }
        if info.folders.is_empty() {
            folders = folders.push(text("None").size(style::SMALL).color(P.fg_dim));
        }
        if vm.armed_remove.is_some() {
            folders = folders.push(
                text("Sessions that pin something in it lose it. Press again to unshare.")
                    .size(style::SMALL)
                    .color(P.warning),
            );
        }
        let full = info.folders.len() >= info.slots;
        folders = folders.push(
            row![quiet_button(
                "Share a folder…",
                (idle && phase == Phase::Ready && !full).then(|| msg(Event::AddFolder)),
            )]
            .padding(iced::Padding {
                top: 6.0,
                ..Default::default()
            }),
        );
        rows.push(wide(
            "Shared Mac folders",
            Some(if full {
                "The VM shares as many folders as it can; unshare one to share another"
            } else {
                "The VM sees each at its own path. Pinning a Mac folder in a session shares it too"
            }),
            folders,
        ));

        // ---- Details
        let size = |bytes: Option<u64>| bytes.map_or_else(|| "–".into(), readable_size);
        let data = |used: Option<u64>| match info.data_size {
            Some(total) => format!("{} of {}", size(used), readable_size(total)),
            None => size(used),
        };
        rows.push(wide(
            "",
            None,
            column![
                crate::daemon_ui::detail_row(
                    "System image",
                    info.image.clone().unwrap_or_else(|| "–".into())
                ),
                crate::daemon_ui::detail_row("System image on Mac", size(info.system_used)),
                crate::daemon_ui::detail_row("Home image on Mac", data(info.home_used)),
                crate::daemon_ui::detail_row("State image on Mac", data(info.state_used)),
                crate::daemon_ui::detail_row("Folder", home_relative(&info.dir)),
            ]
            .spacing(8),
        ));
        rows
    }
}

/// The memory sizes offered, up to half of this Mac's.
const MEMORY_GB: &[u64] = &[2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64];

/// An entry of a resource dropdown: Automatic (`None`) or a number.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    value: Option<u64>,
    label: String,
}

impl Choice {
    fn automatic(label: String) -> Self {
        Self { value: None, label }
    }
    fn value(value: u64, label: String) -> Self {
        Self {
            value: Some(value),
            label,
        }
    }
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn choose<'a>(
    options: Vec<Choice>,
    value: Option<u64>,
    on: impl Fn(Option<u64>) -> Message + 'a,
) -> Element<'a, Message> {
    // a hand-edited value not among the options shows as itself
    let selected = options
        .iter()
        .find(|c| c.value == value)
        .cloned()
        .unwrap_or_else(|| Choice {
            value,
            label: value.map_or_else(String::new, |v| v.to_string()),
        });
    pick_list(options, Some(selected), move |c: Choice| on(c.value))
        .text_size(style::TEXT)
        .padding([6, 12])
        .style(crate::settings_ui::dropdown)
        .menu_style(style::control_menu)
        .into()
}

/// 1.9 GB, 820 MB.
pub fn readable_size(bytes: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    let b = bytes as f64;
    if b >= 10.0 * GB {
        format!("{:.0} GB", b / GB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

fn home_relative(path: &std::path::Path) -> String {
    crate::settings_ui::home_relative(&path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_plainly() {
        assert_eq!(readable_size(820 << 20), "820 MB");
        assert_eq!(readable_size(1_950 << 20), "1.9 GB");
        assert_eq!(readable_size(64 << 30), "64 GB");
    }

    #[test]
    fn resources_keep_only_what_was_chosen() {
        let json = serde_json::to_string(&Resources {
            cpus: Some(4),
            memory_gb: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"cpus":4}"#);
        assert_eq!(
            serde_json::from_str::<Resources>("{}").unwrap(),
            Resources::default()
        );
    }

    #[test]
    fn the_demo_shows_the_section_and_refuses_actions() {
        let (mut app, _) = App::new(true, None);
        assert!(app.vm_shown());
        let _ = app.vm_event(Event::Read);
        assert!(app.vm.info.is_some());
        let _ = app.vm_event(Event::Restart);
        assert!(matches!(app.vm.said, Some(Err(_))));
        let path = PathBuf::from("/Users/someone/GIT/sill");
        let _ = app.vm_event(Event::RemoveFolder(path.clone()));
        assert_eq!(app.vm.armed_remove, Some(path));
    }

    #[test]
    fn storage_warning_and_confirmation() {
        use sill_proto::storage::DiskSpace;
        assert!(low_space(&DiskSpace {
            total: 64 << 30,
            used: 63 << 30,
            available: 0
        }));
        assert!(low_space(&DiskSpace {
            total: 64 << 30,
            used: 60 << 30,
            available: 4 << 30
        }));
        assert!(!low_space(&DiskSpace {
            total: 64 << 30,
            used: 40 << 30,
            available: 24 << 30
        }));
        let (mut app, _) = App::new(true, None);
        let _ = app.vm_event(Event::Read);
        let _ = app.vm_event(Event::ScanCaches);
        let id = "604fd5e1".to_string();
        let _ = app.vm_event(Event::ClearCache(id.clone()));
        assert_eq!(app.vm.armed_clear, Some(id.clone()));
        assert!(app.vm.clearing.is_none());
        let _ = app.vm_event(Event::CancelClear);
        assert!(app.vm.armed_clear.is_none());
        app.vm.caches.as_mut().unwrap().as_mut().unwrap().caches[0].active = true;
        let _ = app.vm_event(Event::ClearCache(id));
        assert!(app.vm.armed_clear.is_none());
        assert!(app.vm.clearing.is_none());
    }

    #[test]
    #[ignore = "writes VM storage PNG for review"]
    fn render_vm_storage() {
        let (mut app, _) = App::new(true, None);
        let _ = app.vm_event(Event::Read);
        let _ = app.vm_event(Event::ScanCaches);
        let disk = &mut app.vm.space.as_mut().unwrap().as_mut().unwrap().state;
        disk.used = 62 << 30;
        disk.available = 1 << 30;
        let _ = app.vm_event(Event::ClearCache("604fd5e1".into()));
        let content = container(app.vm_storage()).padding(20).width(Fill);
        let mut ui =
            iced_test::Simulator::with_size(crate::snapshots::settings(), (680.0, 600.0), content);
        let snapshot = ui.snapshot(&crate::style::theme()).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sill-gui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        // Like the other manual renders, write a fresh review image rather than a golden test.
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("vm-storage-")
            {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        assert!(snapshot.matches_image(dir.join("vm-storage")).unwrap());
    }

    #[test]
    #[ignore = "writes VM settings PNGs for review"]
    fn render_vm_settings() {
        let (mut app, _) = App::new(true, None);
        let _ = app.update(Message::ShowPage(crate::Page::Settings));
        let _ = app.vm_event(Event::Read);
        if let Some(page) = &mut app.settings_page {
            page.category = crate::settings_ui::Category::System;
        }
        crate::snapshots::render(&app, "vm-settings");
        let _ = app.vm_event(Event::RemoveFolder(PathBuf::from("/Volumes/Backup/photos")));
        if let Some(info) = &mut app.vm.info {
            info.chosen.memory_gb = Some(8);
        }
        // the section's lower half: the daemon's card folded
        if let Some(page) = &mut app.settings_page {
            page.collapsed.insert("Daemon");
        }
        crate::snapshots::render_size(&app, "vm-narrow", (420.0, 900.0));
    }
}

/// Warn below 10% available, or below 2 GiB even on a large disk.
fn low_space(disk: &sill_proto::storage::DiskSpace) -> bool {
    disk.available < (2 << 30) || disk.available < disk.total / 10
}
fn space_label(disk: &sill_proto::storage::DiskSpace) -> String {
    format!(
        "{} available · {} used of {}",
        readable_size(disk.available),
        readable_size(disk.used),
        readable_size(disk.total)
    )
}
pub(crate) fn storage_error(why: &str) -> bool {
    why.contains("No space left on device") || why.contains("os error 28")
}
