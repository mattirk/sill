mod agents_ui;
mod providers_ui;
mod annotation;
mod armed;
mod annotation_state;
#[cfg(test)]
mod annotation_tests;
mod box_ui;
mod boxes_ui;
mod changes_page;
mod changes_ui;
mod control;
mod crosscheck;
mod daemon_ui;
mod demo;
mod desktop;
mod desktop_view;
mod host_control;
mod host_runs;
mod details;
mod diff_ui;
mod editor;
mod fonts;
mod forms;
mod geometry;
mod herring;
mod hotkey;
mod icons;
mod keys;
mod layout;
mod lists;
mod login_ui;
#[cfg(target_os = "macos")]
mod local_vm;
#[cfg(target_os = "macos")]
mod mac_screen;
#[cfg(target_os = "macos")]
mod mac_input;
#[cfg(target_os = "macos")]
mod mac_procs;
#[cfg(target_os = "macos")]
mod macos;
mod media;
mod menu;
mod menu_page;
mod names;
mod network;
#[cfg(test)]
mod perf;
mod permissions;
mod policies;
mod processes;
mod rows;
mod selection;
mod session;
mod settings;
mod settings_page;
mod settings_ui;
mod shell;
mod start_ui;
mod shortcuts;
#[cfg(test)]
mod snapshots;
mod style;
mod syntax;
mod terminal;
#[cfg(test)]
mod tests;
mod tool_runs;
mod tray;
mod ui;
mod usage;
mod viewer;
mod vm_ui;
mod window;
mod workspaces;
mod workspaces_ui;
mod files;
mod find;
mod folds;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use iced::widget::{markdown, operation, scrollable};
use iced::{Font, Subscription, Task};
use serde_json::json;
use sill_client::actions::{Actions, Outcome};
use sill_client::conversation::item_text;
use sill_client::files::FilesState;
use sill_client::lane::{self, Lane, Status};
use sill_client::outbox::{Outbox, Settled};
use sill_client::supervision::Attention;
use sill_client::{Change, Connector, Conversation};
use sill_proto::sessions::Session;

#[derive(Parser)]
#[command(about = "Sill's desktop window")]
struct Options {
    /// Open an existing session by exact ID, unique prefix or title.
    #[arg(long, conflicts_with = "demo")]
    session: Option<String>,
    /// Show a synthetic streaming conversation without connecting to a daemon.
    #[arg(long)]
    demo: bool,
    /// Start without showing the window (the sill-gui service): `sill toggle`, the hotkey or
    /// starting sill-gui again shows it.
    #[arg(long)]
    hidden: bool,
}

/// What the control socket received, for the window to do (`control.rs`); taken once by the app.
static CONTROL: std::sync::Mutex<Option<async_channel::Receiver<control::Command>>> =
    std::sync::Mutex::new(None);
/// The window started hidden.
static HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> iced::Result {
    // ssh started this executable to ask the person something during a sign-in (`box_ui`)
    if sill_proto::boxlink::is_askpass() {
        sill_proto::boxlink::askpass_main();
    }
    prefer_low_power_gpu();
    prefer_vulkan();
    let options = Options::parse();
    // one sill-gui per account: a second start shows the running one and leaves
    if !options.demo {
        let (tx, rx) = async_channel::unbounded();
        match control::claim(move |command| tx.send_blocking(command).is_ok()) {
            Ok(Some(())) => *CONTROL.lock().unwrap() = Some(rx),
            Ok(None) => {
                if !options.hidden && !control::send(control::Command::Show) {
                    eprintln!(
                        "sill-gui: another sill-gui holds the control socket but did not answer"
                    );
                }
                return Ok(());
            }
            Err(e) => eprintln!(
                "sill-gui: no control socket ({e}): `sill toggle` and a second start cannot reach this window"
            ),
        }
    }
    HIDDEN.store(options.hidden, std::sync::atomic::Ordering::Relaxed);
    // this window shows session permission state (its Permissions panel), so it may change
    // choices and operate sessions using Desktop development capabilities (docs/protocol.md)
    sill_proto::client::declare_capability(permissions::CAPABILITY);
    sill_proto::client::declare_capability(host_control::CAPABILITY);
    // a second opinion on a workspace's work, on the shelf above the prompt (docs/crosscheck.md)
    sill_proto::client::declare_capability(sill_proto::crosscheck::CAPABILITY);
    fonts::install();
    // the Mac's daemon runs in a VM of the window's own
    #[cfg(target_os = "macos")]
    if local_vm::wanted(options.demo) {
        local_vm::begin();
        local_vm::shut_down_on_signals();
    }
    let above = !options.demo && settings::load().always_on_top();
    let ran = iced::application(
        move || App::new(options.demo, options.session.clone()),
        App::update,
        App::view,
    )
    .title("Sill")
    .theme(|_: &App| style::theme())
    // Interface scale (Settings): on top of the desktop's own, which iced multiplies in
    .scale_factor(|app: &App| app.settings.scale())
    .subscription(App::subscription)
    .default_font(Font::with_name(fonts::SANS_FAMILY))
    .window(iced::window::Settings {
        size: START_SIZE,
        // no title bar: the top bar's empty space moves the window
        // the Mac keeps the system's frame for its rounded corners, rim and shadow, the title
        // bar transparent and its buttons hidden (`macos.rs`)
        decorations: cfg!(target_os = "macos"),
        visible: !options.hidden,
        // closing hides: the window stays, and with it the hotkey and `sill toggle`
        exit_on_close_request: options.demo,
        level: if above {
            iced::window::Level::AlwaysOnTop
        } else {
            iced::window::Level::Normal
        },
        // the application icon, for the panel's window list and the window switcher
        icon: tray::rgba(tray::APP_ICON, 128)
            .and_then(|rgba| iced::window::icon::from_rgba(rgba, 128, 128).ok()),
        // the window's class, `sill` as `sill.desktop`: the panel and menu match the window to it
        #[cfg(target_os = "linux")]
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: "sill".into(),
            ..Default::default()
        },
        #[cfg(target_os = "macos")]
        platform_specific: macos::platform_specific(),
        ..iced::window::Settings::default()
    })
    .run();
    // however the window ended, its VM is shut down cleanly before the process goes
    #[cfg(target_os = "macos")]
    local_vm::shut_down();
    ran
}

/// The window's size as made, in its own units (iced makes it this much times Interface scale).
const START_SIZE: iced::Size = iced::Size::new(720.0, 860.0);

/// The window stays open all day: draw on the integrated GPU and never wake a discrete one, unless
/// the person asks otherwise (`WGPU_POWER_PREF=high`). iced 0.14 asks wgpu for the
/// high-performance adapter, and this variable is the only way to change that.
fn prefer_low_power_gpu() {
    if std::env::var_os("WGPU_POWER_PREF").is_none() {
        // SAFETY: the first thing main does, before any thread exists that could read the
        // environment.
        unsafe { std::env::set_var("WGPU_POWER_PREF", "low") };
    }
}

/// Draw through Vulkan, unless the person names backends (`WGPU_BACKEND`). Where no GPU can be
/// used — no `/dev/dri`, as in a jail or a VM without one — wgpu still offers its GL adapter
/// through Mesa's EGL. iced accepts it, since a surface made only to check compatibility passes,
/// and the window's own surface then fails to configure: wgpu panics, and iced's fallback to the
/// software renderer never runs. With Vulkan only, the missing adapter is an error iced catches,
/// and the window opens on the software renderer. A GPU without Vulkan draws in software too.
fn prefer_vulkan() {
    if cfg!(target_os = "linux") && std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: as in prefer_low_power_gpu, before any thread exists.
        unsafe { std::env::set_var("WGPU_BACKEND", "vulkan") };
    }
}

/// A session shown as a tab: every one that has not ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: String,
    pub title: String,
    pub agent: String,
    pub running: bool,
}

impl From<&Session> for Tab {
    fn from(s: &Session) -> Self {
        Self {
            id: s.session_id.clone(),
            title: s.title.clone(),
            agent: s.agent.clone(),
            running: s.running,
        }
    }
}

/// One entry of an ACP control's dropdown: a mode, an option's choice, or On/Off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    value: String,
    title: String,
    boolean: Option<bool>,
}

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.title)
    }
}

/// What a request the person made was, for its outcome.
#[derive(Debug, Clone)]
pub enum Act {
    /// A keep, revert or undo on the Changes page, by what it does.
    Changes(&'static str),
    Create,
    Reopen(String),
    End(String),
    Delete,
    Resume,
    Send(String),
    Check(String),
    Stop,
    Answer(String),
    Unqueue,
    SendNow,
    Login,
    CarryOn,
    Control,
}

/// The herring's backend pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Agents,
    Changes,
    Settings,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// An update from a session's lane: the session, the lane it came from, the update.
    Lane(String, u64, lane::Update),
    Listed(Result<Vec<Session>, String>),
    /// The outcome of a request the person made in a session.
    Done(String, Act, Outcome),
    Choose(String),
    Details(details::Event),
    Name(names::Event),
    Refresh,
    Daemon(daemon_ui::Event),
    Copy(String),
    /// Copy the open file (read when pressed, not per frame).
    CopyFile,
    Link(String),
    /// A selection drag in the conversation started (`true`) or ended (`false`).
    Selecting(bool),
    DemoTick,
    Tick,
    NetworkPoll,
    /// Refresh the visible session's file tree and Git counts.
    FilesPoll,
    /// Managed processes and exposed ports (`processes.rs`).
    Procs(processes::Event),
    /// The session's Permissions panel (`permissions.rs`).
    Perms(permissions::Event),
    HostControl(host_control::Event),
    /// The box's shared connection: sign-in, retry and SSH's questions.
    Box(box_ui::Event),
    /// Settings' Remote boxes: add, open, disconnect and remove saved boxes (`boxes_ui`).
    Boxes(boxes_ui::Event),
    /// The live view of a session's test desktop (`desktop_view.rs`).
    Desktop(desktop_view::Event),
    NetworkRead(String, Result<sill_proto::admin::NetworkDenied, String>),
    NetworkAnswer(String, String, bool, bool),
    NetworkDone(String, network::Choice, Outcome),
    /// A network choice's note has had its time (its number, so a later note stays).
    NetworkNoticeExpired(String, u64),
    /// What the person does in a session's own view, with that session's id (`session.rs`).
    Session(String, session::Event),
    /// Ctrl+End: the selected session's conversation to its end. The keys' messages mean the
    /// session selected when the key arrives.
    Latest,
    /// The login card of a session (`login_ui`).
    Login(String, login_ui::Event),
    /// A press on the top bar's empty space: the window manager moves the window (not in Shelf
    /// mode, which is locked to its edge).
    DragWindow,
    ResizeWindow(iced::window::Direction),
    /// Time to read the displays and the window again (`geometry.rs`).
    GeometryTick,
    /// The displays and the window as read, and why they were.
    Geometry(Read, Option<(Vec<geometry::Display>, Option<geometry::Rect>)>),
    /// The window was placed: the displays it was placed with, and where it went.
    Placed(Option<(Vec<geometry::Display>, Option<geometry::Target>)>),
    /// Placed for a mode change (`switching`): placed again when the mode moved on meanwhile.
    SwitchPlaced(Option<(Vec<geometry::Display>, Option<geometry::Target>)>),
    /// The window manager moved the window.
    Moved,
    Quit,
    /// The window's own buttons on the Mac (the `+` menu): into the Dock, and to the size the
    /// system thinks best and back.
    Minimize,
    Zoom,
    Media(media::Event),
    Annotation(annotation::Event),
    AttachmentPaths(String, String, Option<Vec<std::path::PathBuf>>),
    /// The shell shortcuts: the selected session's jail shell in place of the prompt, or the
    /// prompt again.
    ToggleTerminal,
    ToggleHostTerminal,
    Terminal(String, u64, terminal::Event),
    TermPaste(String, u64, Option<String>),
    Files(files::Action),
    /// The window's new width.
    Resized(iced::Size),
    /// The window is open: ask for its monitor's size.
    Opened(iced::window::Id),
    /// The window's X11 id, for placing and activating it; none when it is no X11 window.
    XId(Option<u64>),
    /// From the control socket: show, hide, toggle, quit.
    Control(control::Command),
    /// From the hotkey's thread: a press, or how the grab stands.
    Hotkey(hotkey::Report),
    /// A toggle on a shown window: hidden when it is the active one, else raised.
    ToggleDecided(bool, u32),
    /// The window manager asked to close the window: it hides.
    CloseRequested,
    /// From the tray icon.
    Tray(tray::Asked),
    /// The Mac's menu bar icon: the next frame while an agent works.
    #[cfg(target_os = "macos")]
    TrayTick,
    /// Work done off the window's thread that needs no answer.
    Nothing,
    /// The monitor's size as iced reports it: Shelf mode's width where there is no X11.
    Monitor(Option<iced::Size>),
    /// Interface scale one preset larger (1) or smaller (-1), or back to 100 % (0).
    ScaleStep(i32),
    /// The pointer came over the top bar's activity, or left it: the timer shows meanwhile.
    ActivityHover(bool),
    /// Full conversation, and the drawer's edge dragged.
    Layout(layout::Action),
    /// Tab (true) or Shift+Tab: the keyboard moves to the next or previous control.
    Focus(bool),
    /// Start a session with the current agent, or open the chooser with no selected tab.
    NewSession,
    CloseCurrent,
    /// Zero-based position among the visible session tabs.
    SelectTab(usize),
    /// Ctrl+period (1) or comma (-1): the next or previous session.
    StepTab(i32),
    /// Escape: between the session and the backend.
    Escape,
    /// PageDown (true) or PageUp in the conversation or the open file.
    Page(bool),
    /// The `+` menu: start, reopen, delete sessions.
    Menu(menu::Event),
    /// The herring: the Changes page, or back to the conversation.
    ToggleChanges,
    Changes(changes_ui::Event),
    /// A session's workspaces (docs/parallel-coding.md §12).
    Workspaces(workspaces::Event),
    Crosscheck(crosscheck::Event),
    /// The herring's pages: one shown in the conversation's place.
    ShowPage(Page),
    /// The agents a session can start with, read for the window with no open session.
    StartAgents(Result<Vec<sill_proto::agent_admin::Definition>, String>),
    Settings(settings_ui::Event),
    /// The Settings page's Virtual machine section (`vm_ui.rs`).
    Vm(vm_ui::Event),
    /// A reply belongs to the Settings page that issued it, even across navigation.
    SettingsReply(String, settings_ui::Event),
    Agents(agents_ui::Event),
    /// A registry agent's icon arrived from the daemon (None: it has none).
    AgentIcon(String, Option<Vec<u8>>),
    /// The daemon's settings, read at the start and with each refresh: the theme, the timer.
    DaemonSettings(u64, Result<sill_proto::admin::Settings, String>),
    /// A tab's ×: end its session (a ghost's needs a second press).
    CloseTab(String),
    Find(find::Event),
    Noop,
}

pub struct Rendered {
    pub source: String,
    pub markdown: markdown::Content,
    leaves: std::sync::OnceLock<Vec<String>>,
}

impl Rendered {
    pub fn leaves(&self) -> &[String] {
        self.leaves.get_or_init(|| {
            let mut out = Vec::new();
            viewer::markdown_leaves(self.markdown.items(), &mut out);
            out
        })
    }
}

/// Everything that belongs to one open session, for as long as it is open in this window: its
/// lane and conversation, what the person typed and sent, its terminal, files and scroll position.
/// Dropping it ends the lane and detaches the terminal; the session goes on.
pub struct SessionView {
    pub id: String,
    /// When the working clock came into view, for its slide out from under the spinner; set
    /// while drawing (the view has no other way to see the clock appear).
    pub clock_shown: std::cell::Cell<Option<Instant>>,
    lane: Option<Lane>,
    lane_epoch: u64,
    pub status: Status,
    pub conversation: Conversation,
    pub rendered: BTreeMap<String, Rendered>,
    pub draft: editor::Editor,
    pub attachments: Vec<media::DraftAttachment>,
    pub annotation: Option<annotation::Editor>,
    pub pending_attachments: BTreeMap<String, Vec<media::DraftAttachment>>,
    pub attachments_busy: bool,
    pub media_token: String,
    pub media: media::Cache,
    pub media_requests: BTreeMap<String, String>,
    pub image_viewer: Option<String>,
    pub image_viewer_mode: Option<(iced::window::Id, iced::window::Mode)>,
    pub outbox: Outbox,
    /// A request the person made that did not take effect; shown until the next one.
    pub failed: Option<String>,
    pub open_tools: BTreeSet<String>,
    /// Groups of tool calls shown call by call, by their first call's item.
    pub open_groups: BTreeSet<String>,
    pub open_answers: BTreeSet<String>,
    /// Answers on their way, so a second press does not send another.
    pub answering: BTreeSet<String>,
    pub follow_tail: bool,
    pub scroll_offset: f32,
    /// The conversation's visible height, once it has scrolled (0 until then).
    pub viewport: f32,
    /// Each row's measured height (`rows.rs`): rows far from the view are left out as gaps.
    pub heights: rows::Heights,
    pub terminal: Option<terminal::Pane>,
    pub terminal_shown: bool,
    pub terminal_host: bool,
    parked_terminal: Option<(terminal::Pane, u64)>,
    terminal_generation: u64,
    /// The agent's login under way, if any (`login_ui`).
    login: login_ui::Login,
    pub files: FilesState,
    pub reader: Option<files::ReaderView>,
    /// The failed turn the person has looked at, so its mark goes.
    seen_failure: Option<String>,
    /// What the person has entered in open form questions.
    pub forms: BTreeMap<String, forms::Form>,
    /// Its managed processes and the ports exposed from them (`processes.rs`).
    pub processes: processes::Procs,
    /// Its permission state and panel (`permissions.rs`).
    pub permissions: permissions::Perms,
    /// Its test desktop's live view (`desktop_view.rs`).
    pub desktop: desktop_view::View,
    pub host_control: host_control::View,
    /// Its workspaces, the shelf's offers and the chips' marks (`workspaces.rs`).
    pub workspaces: workspaces::State,
}

impl SessionView {
    fn select_shell(&mut self, host: bool) {
        if self.terminal_host != host {
            let old = self
                .terminal
                .take()
                .map(|pane| (pane, self.terminal_generation));
            if let Some((pane, generation)) = self.parked_terminal.take() {
                self.terminal = Some(pane);
                self.terminal_generation = generation;
            }
            self.parked_terminal = old;
            self.terminal_host = host;
            self.terminal_shown = false;
        }
    }
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            clock_shown: std::cell::Cell::new(None),
            lane: None,
            lane_epoch: 0,
            status: Status::Closed,
            conversation: Conversation::default(),
            rendered: BTreeMap::new(),
            draft: editor::Editor::new(),
            attachments: vec![],
            annotation: None,
            pending_attachments: BTreeMap::new(),
            attachments_busy: false,
            media_token: sill_client::request_id("media-view"),
            media: Default::default(),
            media_requests: Default::default(),
            image_viewer: None,
            image_viewer_mode: None,
            outbox: Outbox::default(),
            failed: None,
            open_tools: BTreeSet::new(),
            open_groups: BTreeSet::new(),
            open_answers: BTreeSet::new(),
            answering: BTreeSet::new(),
            follow_tail: true,
            scroll_offset: 0.0,
            viewport: 0.0,
            heights: Default::default(),
            terminal: None,
            terminal_shown: false,
            terminal_host: false,
            parked_terminal: None,
            terminal_generation: 0,
            login: login_ui::Login::default(),
            files: FilesState::default(),
            reader: None,
            seen_failure: None,
            forms: BTreeMap::new(),
            processes: processes::Procs::default(),
            permissions: permissions::Perms::default(),
            desktop: desktop_view::View::default(),
            host_control: host_control::View::default(),
            workspaces: workspaces::State::default(),
        }
    }

    /// The conversation is on screen (no file over it).
    pub fn conversation_shown(&self) -> bool {
        self.files.reader.is_none() && self.annotation.is_none()
    }

    pub fn attention(&self) -> Attention {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut attention = Attention::of(&self.conversation, &self.outbox, &self.status, now);
        if attention.failed.is_some()
            && self.seen_failure.as_deref() == self.conversation.last_turn_id()
        {
            attention.failed = None;
        }
        attention
    }

    /// The session's work is sill's summary turn (session state `summarizing`): its tab shows
    /// only the agent glyph.
    pub fn summarizing(&self) -> bool {
        self.conversation
            .session
            .as_ref()
            .is_some_and(|s| s.summarizing.is_some())
    }

    /// The person has looked at this session: a failure shown is no longer news.
    fn seen(&mut self) {
        self.seen_failure = self.conversation.last_turn_id().map(str::to_owned);
    }

    /// Every item's rendering, after a snapshot.
    fn rebuild(&mut self) {
        self.rendered.clear();
        let ids: Vec<String> = self.conversation.items().keys().cloned().collect();
        for id in ids {
            self.render(&id);
        }
    }

    /// One item's rendering, parsed incrementally while its text only grows.
    fn render(&mut self, id: &str) {
        let source = match self.conversation.item(id) {
            None => {
                self.rendered.remove(id);
                return;
            }
            // a tool row is drawn from its fields, not as Markdown
            Some(item) if item.kind == "tool" => return,
            Some(item) => {
                let mut text_item = item.clone();
                text_item.content.retain(|block| block.kind != "image");
                item_text(&text_item)
            }
        };
        let previous_images: BTreeSet<String> = self.rendered.get(id)
            .map(|r| r.markdown.images().iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        match self.rendered.get_mut(id) {
            Some(cached) if cached.source == source => {}
            Some(cached) if source.starts_with(&cached.source) => {
                cached.markdown.push_str(&source[cached.source.len()..]);
                cached.source = source;
                cached.leaves = std::sync::OnceLock::new();
            }
            _ => {
                self.rendered.insert(
                    id.to_string(),
                    Rendered {
                        markdown: markdown::Content::parse(&source),
                        source,
                        leaves: std::sync::OnceLock::new(),
                    },
                );
            }
        }
        // A new mention of a mutable file must not reuse an earlier message's preview.
        if let Some(rendered) = self.rendered.get(id) {
            for url in rendered.markdown.images() {
                let key = url.to_string();
                if !previous_images.contains(&key) {
                    self.media.remove(&key);
                    self.media_requests.remove(&key);
                }
            }
        }
    }

    /// Apply one event; whether the conversation grew or changed visibly.
    fn accept(&mut self, event: sill_proto::sfp::SessionEvent) -> bool {
        match self.conversation.accept(event) {
            Ok(Some(change)) => {
                match &change {
                    Change::Item(id) => self.render(id),
                    Change::ItemRemoved(id) => {
                        self.rendered.remove(id);
                    }
                    Change::Submission(_) => {
                        self.outbox.reconcile(&self.conversation);
                    }
                    Change::Question(id) => {
                        if self
                            .conversation
                            .questions
                            .get(id)
                            .is_some_and(|q| q.state != "open")
                        {
                            self.answering.remove(id);
                        }
                    }
                    Change::Session | Change::Turn(_) => {}
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.failed = Some(format!("{error:#}. Restoring the conversation."));
                if let Some(lane) = &self.lane {
                    lane.resync();
                }
                false
            }
        }
    }

    /// The session ended or was deleted, as its own state says.
    fn ended(&self) -> bool {
        self.conversation
            .session
            .as_ref()
            .is_some_and(|s| s.lifecycle == "ended" || s.deleted)
    }
}

pub struct App {
    network: network::State,
    /// Numbers the notes on exposed ports, so a later one outlives an earlier one's expiry.
    process_serial: u64,
    permission_serial: u64,
    desktop_serial: u64,
    pub demo: bool,
    daemon: daemon_ui::State,
    /// The Mac window's own VM as Settings shows it (`vm_ui.rs`).
    vm: vm_ui::State,
    /// The box's shared SSH connection: sign-in state and SSH's questions (`box_ui`).
    box_link: box_ui::State,
    /// The saved boxes and the Add a box form (`boxes_ui`).
    boxes: boxes_ui::State,
    connect: Option<Connector>,
    actions: Option<Arc<Actions>>,
    /// Where every lane reports, tagged with its session and epoch.
    lanes: Option<async_channel::Sender<(String, u64, lane::Update)>>,
    next_lane: u64,
    next_terminal: u64,
    pub tabs: Vec<Tab>,
    /// Every session the daemon has, newest first (the menu's Reopen list).
    pub all_sessions: Vec<Session>,
    pub views: BTreeMap<String, SessionView>,
    /// The `+` menu, while open.
    pub menu: Option<menu::Menu>,
    /// The Changes page, while shown in place of the conversation.
    pub changes: Option<changes_ui::ChangesPage>,
    /// What the Changes page lists, as last read: kept while the page is closed, so opening it
    /// shows that at once while the new listing is read.
    pub changes_listing: changes_ui::Listing,
    /// Every workspace's Crosscheck review, and the Reviewer setting (`crosscheck`).
    pub crosscheck: crosscheck::State,
    name_editor: Option<names::Editor>,
    details: Option<details::Page>,
    details_drafts: BTreeMap<String, details::Draft>,
    pub settings_page: Option<settings_ui::SettingsPage>,
    pub agents_page: agents_ui::AgentsPage,
    /// Registry agents' icons (SVG bytes) by agent id, for tabs and the + menu; built-ins have
    /// their own glyphs.
    pub agent_icons: BTreeMap<String, Vec<u8>>,
    /// The agents a session can start with, for the window with no open session; `None` until
    /// read.
    pub start_agents: Option<Result<Vec<sill_proto::agent_admin::Definition>, String>>,
    start_agents_pending: bool,
    agent_icons_queued: BTreeSet<String>,
    /// The herring's page last shown: what it opens next.
    pub last_page: Page,
    /// The daemon's settings as last read.
    pub daemon_settings: Option<sill_proto::admin::Settings>,
    /// A session just created or reopened: chosen once the list shows it.
    pending_choose: Option<String>,
    /// A ghost's tab whose × was pressed once.
    pub armed_close: armed::Armed<String>,
    pub selected: Option<String>,
    /// Open tabs in activation order, most recently visited last.
    tab_history: Vec<String>,
    /// `--session`, chosen when the first list arrives.
    initial: Option<String>,
    /// Trouble reading the daemon's session list.
    pub list_error: Option<String>,
    pub listed: bool,
    /// One list read at a time; explicit changes request a fresh read after it finishes.
    list_pending: bool,
    list_again: bool,
    settings_pending: bool,
    settings_revision: u64,
    pub drawer: files::Drawer,
    /// Set by each tick while the agent is busy; the spinner's angle comes from it.
    pub now: Instant,
    demo_step: usize,
    /// The window's width, for the narrow layout's choices.
    pub width: f32,
    pub height: f32,
    pub layout: layout::Layout,
    /// sill-gui's own settings (`settings.rs`); saved only when `persist` (not in the demo).
    pub settings: settings::Settings,
    persist: bool,
    /// The monitor's size in the window's units, as iced reports it: Shelf mode's width where
    /// there is no X11 to place the window with.
    pub monitor: Option<iced::Size>,
    /// Where the window goes and what the person made of it (`geometry.rs`).
    pub geometry: geometry::Controller,
    /// The window, once open, and its X11 id; `no_x11` once it is known to have none.
    pub window_id: Option<iced::window::Id>,
    pub xid: Option<u64>,
    no_x11: bool,
    /// The window is shown (not hidden by the hotkey, `sill toggle` or Close).
    pub visible: bool,
    /// A change into or out of Shelf mode under way on X11: the window read, then placed for this
    /// mode. Until it lands another change only sets the mode; the window is placed for that mode
    /// once this one lands. Two reads in flight at once kept one mode's geometry as the other's.
    pub switching: Option<layout::Mode>,
    /// The global hotkey's thread (none in the demo and the tests), and how its grab stands.
    hotkey: Option<hotkey::Hotkey>,
    pub hotkey_status: Option<String>,
    /// The tray icon while the shared `tray` setting has it, and where its clicks go.
    tray: Option<tray::Tray>,
    tray_asked: Option<async_channel::Sender<tray::Asked>>,
    /// What the tray was last told: an agent works.
    tray_busy: bool,
    /// The pointer is over the top bar's activity (the timer shows then).
    pub activity_hovered: bool,
    /// When the view last went between the session and the backend: the herring swims a stroke.
    pub herring_flick: Option<Instant>,
    /// A selection is being dragged in the conversation: following the tail would pull the text
    /// from under the pointer, so new text does not scroll it until the drag ends.
    pub selecting: bool,
    pub find: find::State,
}

/// Why the window's geometry was read: the regular look, or the last one before the window
/// hides or leaves a mode (the person's change is kept then, not waited for).
#[derive(Debug, Clone, Copy)]
pub enum Read {
    Poll,
    Hide,
    Leave(layout::Mode),
}

/// The window's X11 id, when it is an X11 window: on Wayland iced's raw id is a surface's address,
/// which X11 calls must never be given (they could move another client's window).
fn x11_window(window: &dyn iced::window::Window) -> Option<u64> {
    use iced::window::raw_window_handle::RawWindowHandle;
    match window.window_handle().ok()?.as_raw() {
        // X ids are 32 bits, whatever Xlib's long is
        RawWindowHandle::Xlib(handle) => Some(u64::from(handle.window as u32)),
        RawWindowHandle::Xcb(handle) => Some(u64::from(handle.window.get())),
        _ => None,
    }
}

/// Run `work` on a thread of its own; its result comes back as a message.
fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) -> Message + Send + 'static,
) -> Task<Message> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("sill-gui-call".into())
        .spawn(move || {
            let _ = tx.send(work());
        });
    if spawned.is_err() {
        return Task::none();
    }
    Task::perform(rx, move |result| match result {
        Ok(value) => done(value),
        Err(_) => Message::Noop,
    })
}

impl App {
    fn new(demo: bool, initial: Option<String>) -> (Self, Task<Message>) {
        #[cfg(target_os = "macos")]
        let connect = if local_vm::phase().is_some() {
            local_vm::connector()
        } else {
            sill_client::connect()
        };
        #[cfg(not(target_os = "macos"))]
        let connect = sill_client::connect();
        Self::with(demo, initial, (!demo).then_some(connect))
    }

    /// The app on `connect`'s daemon (none in `--demo`).
    pub fn with(
        demo: bool,
        initial: Option<String>,
        connect: Option<Connector>,
    ) -> (Self, Task<Message>) {
        let (drawer, file_task) = files::Drawer::new(connect.clone());
        let mut app = Self {
            daemon: daemon_ui::State::default(),
            vm: vm_ui::State::default(),
            box_link: box_ui::State::default(),
            boxes: boxes_ui::State::default(),
            network: network::State::default(),
            process_serial: 0,
            permission_serial: 0,
            desktop_serial: 0,
            demo,
            actions: connect.clone().map(Actions::new),
            connect,
            lanes: None,
            next_lane: 0,
            next_terminal: 0,
            tabs: Vec::new(),
            all_sessions: Vec::new(),
            views: BTreeMap::new(),
            menu: None,
            changes: None,
            name_editor: None,
            details: None,
            details_drafts: BTreeMap::new(),
            settings_page: None,
            agents_page: agents_ui::AgentsPage::default(),
            agent_icons: BTreeMap::new(),
            start_agents: None,
            start_agents_pending: false,
            agent_icons_queued: BTreeSet::new(),
            last_page: Page::Changes,
            changes_listing: changes_ui::Listing::default(),
            crosscheck: crosscheck::State::default(),
            daemon_settings: None,
            pending_choose: None,
            armed_close: Default::default(),
            selected: None,
            tab_history: Vec::new(),
            initial,
            list_error: None,
            listed: false,
            list_pending: false,
            list_again: false,
            settings_pending: false,
            settings_revision: 0,
            drawer,
            now: Instant::now(),
            demo_step: 0,
            width: START_SIZE.width,
            height: START_SIZE.height,
            layout: layout::Layout {
                // the Mac's window is the system's: resized and placed by it, without modes
                plain: cfg!(target_os = "macos") && !cfg!(test),
                ..layout::Layout::default()
            },
            settings: settings::Settings::default(),
            persist: false,
            monitor: None,
            geometry: geometry::Controller::default(),
            window_id: None,
            xid: None,
            no_x11: false,
            visible: !HIDDEN.load(std::sync::atomic::Ordering::Relaxed),
            switching: None,
            hotkey: None,
            hotkey_status: None,
            tray: None,
            tray_asked: None,
            tray_busy: false,
            activity_hovered: false,
            herring_flick: None,
            selecting: false,
            find: find::State::default(),
        };
        if demo {
            // the demo keeps its own state, but shows at the person's Interface scale (read
            // only, never written; not in tests, which start from the defaults)
            if !cfg!(test) {
                app.settings.interface_scale = settings::load().interface_scale;
            }
            let tab = Tab {
                id: "demo".into(),
                title: "witty-willow".into(),
                agent: "claude".into(),
                running: true,
            };
            let mut view = SessionView::new("demo");
            view.conversation = demo::conversation();
            view.status = Status::Live;
            view.rebuild();
            app.tabs = vec![tab];
            app.views.insert("demo".into(), view);
            app.selected = Some("demo".into());
            app.tab_history.push("demo".into());
            app.listed = true;
            let files = app.files_refresh("demo");
            return (app, Task::batch([file_task, files]));
        }
        // the window's mode and sizes as it last had them (not in the demo or tests)
        if !demo && !cfg!(test) && app.connect.is_some() {
            app.settings = settings::load();
            app.persist = true;
        }

        app.layout.mode = app
            .settings
            .mode
            .as_deref()
            .and_then(layout::Mode::from_name)
            .unwrap_or_default();
        app.layout.drawer = app.settings.drawer_width;
        app.drawer.shown = app.layout.mode == layout::Mode::Drawer;
        if app.layout.plain {
            // one layout: the reading measure, the drawer when asked for
            app.layout.mode = layout::Mode::Drawer;
            app.drawer.shown = false;
        }
        // the control socket's commands, and the hotkey (a real window only)
        let mut desktop_tasks = Vec::new();
        if let Some(control) = CONTROL.lock().unwrap().take() {
            let commands = iced::futures::stream::unfold(control, |rx| async move {
                rx.recv().await.ok().map(|command| (command, rx))
            });
            desktop_tasks.push(Task::run(commands, Message::Control));
        }
        if !demo && !cfg!(test) {
            let (tx, rx) = async_channel::unbounded();
            app.hotkey = Some(hotkey::start(move |report| {
                tx.send_blocking(report).is_ok()
            }));
            let reports = iced::futures::stream::unfold(rx, |rx| async move {
                rx.recv().await.ok().map(|report| (report, rx))
            });
            desktop_tasks.push(Task::run(reports, Message::Hotkey));
            // the tray's clicks; the icon itself once the settings say whether to show it
            let (tx, rx) = async_channel::unbounded();
            app.tray_asked = Some(tx);
            let asked = iced::futures::stream::unfold(rx, |rx| async move {
                rx.recv().await.ok().map(|asked| (asked, rx))
            });
            desktop_tasks.push(Task::run(asked, Message::Tray));
        }
        if !demo {
            // SSH's questions during a sign-in to the box
            let (tx, rx) = async_channel::unbounded();
            app.box_link.asker = Some(tx);
            let asks = iced::futures::stream::unfold(rx, |rx| async move {
                rx.recv().await.ok().map(|ask| (ask, rx))
            });
            desktop_tasks.push(Task::run(asks, |ask| Message::Box(box_ui::Event::Asked(ask))));
        }
        let (tx, rx) = async_channel::bounded(256);
        app.lanes = Some(tx);
        let lanes = iced::futures::stream::unfold(rx, |rx| async move {
            rx.recv().await.ok().map(|update| (update, rx))
        });
        let list = Task::batch([
            app.list(),
            app.read_daemon_settings(),
            app.daemon_event(daemon_ui::Event::Check),
        ]);
        (
            app,
            Task::batch([
                file_task,
                Task::run(lanes, |(session, epoch, update)| {
                    Message::Lane(session, epoch, update)
                }),
                list,
                Task::batch(desktop_tasks),
            ]),
        )
    }

    /// The prompt takes the keyboard, where a session's conversation is shown.
    fn focus_prompt(&self) -> Task<Message> {
        if self.backend_shown() || self.current().is_none_or(|v| v.terminal_shown || v.annotation.is_some()) {
            return Task::none();
        }
        operation::focus("prompt")
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![];
        if self.demo && self.demo_step <= demo::CHUNKS.len() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(600)).map(|_| Message::DemoTick));
        }
        // the menu bar's icon moves while an agent works; the Mac steps it on the main thread
        // (Linux's tray has a thread of its own)
        #[cfg(target_os = "macos")]
        if self.tray_busy && self.tray.is_some() {
            subscriptions.push(
                iced::time::every(Duration::from_millis(tray::FRAME_MS)).map(|_| Message::TrayTick),
            );
        } else if self.tray.is_some() && local_vm::phase().is_some() {
            // the agent's Mac input starts without a message of its own: looked for here
            subscriptions.push(iced::time::every(Duration::from_millis(400)).map(|_| Message::TrayTick));
        }
        // While busy, update the timer label and its width together once a second.
        // The spinner and timer reveal animate between these samples.
        if self.current().is_some_and(|v| v.conversation.busy()) {
            subscriptions.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        // a Reviewer at work: its line's latest step and time
        if self.crosscheck.ticking() && !self.demo {
            subscriptions.push(iced::time::every(Duration::from_secs(2)).map(|_| Message::Crosscheck(crosscheck::Event::Poll)));
        }
        // the VM on its way somewhere while Settings shows it
        if self.vm_ticking() {
            subscriptions.push(
                iced::time::every(Duration::from_secs(1)).map(|_| Message::Vm(vm_ui::Event::Tick)),
            );
        }
        if !self.demo {
            subscriptions
                .push(iced::time::every(Duration::from_secs(2)).map(|_| Message::NetworkPoll));
            // managed processes: every open session's, so its ports close when they end
            subscriptions.push(
                iced::time::every(Duration::from_secs(3))
                    .map(|_| Message::Procs(processes::Event::Poll)),
            );
        }
        if !self.demo && !self.backend_shown() && self.details.is_none() && self.current().is_some()
        {
            subscriptions.push(
                iced::time::every(Duration::from_secs(2))
                    .with("session-files")
                    .map(|_| Message::FilesPoll),
            );
        }
        // the shown test desktop: a new picture about once a second (an unchanged one is not fetched)
        if !self.demo && self.current().is_some_and(|v| v.desktop.shown) {
            subscriptions.push(
                iced::time::every(Duration::from_secs(1)).map(|_| Message::Desktop(desktop_view::Event::Tick)),
            );
        }
        // sessions started or ended elsewhere appear and go
        if !self.demo {
            subscriptions
                .push(iced::time::every(Duration::from_secs(15)).map(|_| Message::Refresh));
        }
        // the displays and the window, read often while the window settles or the person drags
        // it (`geometry.rs`)
        if self.visible && self.xid.is_some() {
            let every = if self.geometry.busy(Instant::now()) {
                200
            } else {
                1000
            };
            subscriptions.push(
                iced::time::every(Duration::from_millis(every)).map(|_| Message::GeometryTick),
            );
        }
        subscriptions.push(iced::keyboard::listen().filter_map(keys::navigation));
        subscriptions.push(iced::window::resize_events().map(|(_, size)| Message::Resized(size)));
        subscriptions.push(iced::event::listen_with(|event, _status, _window| {
            matches!(event, iced::Event::Window(iced::window::Event::Moved(_)))
                .then_some(Message::Moved)
        }));
        subscriptions.push(self.layout.subscription());
        // the monitor's size once the window is there: Shelf mode's width comes from it
        subscriptions.push(iced::window::open_events().map(Message::Opened));
        subscriptions.push(iced::window::close_requests().map(|_| Message::CloseRequested));
        Subscription::batch(subscriptions)
    }

    /// The drawer is over the conversation rather than beside it.
    pub fn narrow(&self) -> bool {
        matches!(
            self.layout.drawer(self.drawer.shown, self.width),
            layout::Drawer::Over(_)
        )
    }

    pub fn current(&self) -> Option<&SessionView> {
        self.views.get(self.selected.as_deref()?)
    }

    fn current_mut(&mut self) -> Option<&mut SessionView> {
        self.views.get_mut(self.selected.as_deref()?)
    }

    /// A session event from the keys, for the session selected now.
    fn selected_event(&mut self, event: session::Event) -> Task<Message> {
        match self.selected.clone() {
            Some(id) => self.session_event(id, event),
            None => Task::none(),
        }
    }

    /// The session list, read off the GUI thread.
    fn list(&mut self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        if self.list_pending {
            self.list_again = true;
            return Task::none();
        }
        self.list_pending = true;
        blocking(
            move || actions.sessions().map_err(|e| format!("{e:#}")),
            Message::Listed,
        )
    }

    /// A lane for `id`, replacing any it had; updates from an older lane are ignored.
    fn open_lane(&mut self, id: &str, mode: lane::Mode) {
        let (Some(connect), Some(tx)) = (self.connect.clone(), self.lanes.clone()) else {
            return;
        };
        self.next_lane += 1;
        let epoch = self.next_lane;
        let view = self
            .views
            .entry(id.to_string())
            .or_insert_with(|| SessionView::new(id));
        let session = id.to_string();
        view.lane_epoch = epoch;
        view.status = Status::Connecting;
        view.lane = Some(Lane::open(id, mode, connect, move |update| {
            tx.send_blocking((session.clone(), epoch, update)).is_ok()
        }));
    }

    /// The session's jail was started again under this window: attach anew, which starts its
    /// agent (the daemon starts one for an attachment or a prompt, not for the jail alone) and
    /// reads the conversation afresh.
    pub(crate) fn reattach(&mut self, id: &str) {
        if !self.demo && self.views.contains_key(id) {
            self.open_lane(id, lane::Mode::Open);
        }
    }

    /// One request the person made in `session`; its outcome comes back as `Done`.
    fn act(
        &self,
        session: &str,
        act: Act,
        method: &'static str,
        params: serde_json::Value,
    ) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let session = session.to_string();
        blocking(
            move || actions.call(method, params),
            move |outcome| Message::Done(session, act, outcome),
        )
    }

    /// Back to the conversation's last place: its tail when it was following, else its offset.
    fn restore_scroll(&self) -> Task<Message> {
        let Some(view) = self.current() else {
            return Task::none();
        };
        if view.follow_tail {
            operation::snap_to_end("conversation")
        } else {
            operation::scroll_to(
                "conversation",
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: view.scroll_offset,
                },
            )
        }
    }

    /// A backend page shows in the session's place.
    pub fn backend_shown(&self) -> bool {
        self.changes.is_some() || self.settings_page.is_some() || self.agents_page.shown
    }

    /// Every message: its update, and the herring's stroke when the view went between the
    /// session and the backend (by the herring, Escape or anything else).
    fn update(&mut self, message: Message) -> Task<Message> {
        if self.guard_file_edits(&message) {
            return Task::none();
        }
        let before = self.backend_shown();
        let prior_session = self.selected.clone();
        let prior_image_mode = self.current().and_then(|v| v.image_viewer_mode);
        let mut task = self.apply(message);
        // Find belongs to the conversation it was opened on (docs/find.md, decision 3)
        if self.find.shown && (prior_session != self.selected || !self.find_available()) {
            self.find.close();
        }
        if before != self.backend_shown() {
            self.herring_flick = Some(Instant::now());
            if before {
                task = task.chain(self.focus_prompt());
            }
        }
        let hidden = self.backend_shown();
        if hidden || prior_session != self.selected {
            if let Some((id, mode)) = prior_image_mode {
                task = Task::batch([task, iced::window::set_mode(id, mode)]);
            }
            if let Some(view) = prior_session.as_ref().and_then(|id| self.views.get_mut(id)) {
                view.image_viewer = None;
                view.image_viewer_mode = None;
            }
        }
        for view in self.views.values_mut() {
            if (hidden || prior_session != self.selected) && let Some(editor) = &mut view.annotation {
                editor.drawing = false;
            }
            if !view.pending_attachments.is_empty() {
                view.pending_attachments.retain(|id, _| view.outbox.get(id).is_some());
                view.prune_draft_media();
            }
        }
        if let Some(session) = self.selected.clone() {
            task = Task::batch([task, self.reader_anchor(&session)]);
        }
        // the tray moves while any session's agent works
        let busy = self.views.values().any(|v| v.conversation.busy());
        // an agent driving the Mac's mouse and keyboard shows on the menu-bar herring too
        #[cfg(target_os = "macos")]
        let busy = busy || mac_input::active();
        if busy != self.tray_busy {
            self.tray_busy = busy;
            if let Some(tray) = &self.tray {
                tray.set_busy(busy);
            }
        }
        task
    }

    fn apply(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Media(event) => return self.media_event(event),
            Message::Annotation(event) => return self.annotation_event(event),
            Message::AttachmentPaths(session, token, paths) => {
                if self
                    .views
                    .get(&session)
                    .is_some_and(|v| v.media_token == token)
                {
                    return self.attachment_paths(session, paths);
                }
                return Task::none();
            }
            Message::Lane(session, epoch, update) => {
                return self.lane_update(session, epoch, update);
            }
            Message::Listed(result) => return self.listed(result),
            Message::Done(session, act, outcome) => return self.done(session, act, outcome),
            Message::Choose(id) => return self.choose(id),
            Message::Details(event) => return self.details_event(event),
            Message::Name(event) => return self.name_event(event),
            Message::Daemon(event) => return self.daemon_event(event),
            Message::Refresh => {
                // A slow daemon must not accumulate a new thread on each timer tick.
                // Explicit lifecycle requests still queue a follow-up through list().
                let list = if self.list_pending {
                    Task::none()
                } else {
                    self.list()
                };
                let mut tasks = vec![
                    list,
                    self.read_daemon_settings(),
                    self.daemon_event(daemon_ui::Event::Check),
                ];
                if let Some(id) = self.selected.clone() {
                    tasks.push(self.read_permissions(&id));
                }
                if let Some(p) = self.details.as_ref().filter(|p| !p.pending) {
                    tasks.push(self.details_load(p.id.clone()));
                }
                if self
                    .changes
                    .as_ref()
                    .is_some_and(|c| !c.history && !c.loading)
                {
                    tasks.push(self.changes_event(changes_ui::Event::Refresh));
                }
                return Task::batch(tasks);
            }
            Message::Session(id, event) => return self.session_event(id, event),
            Message::Latest => return self.selected_event(session::Event::Latest),
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::CopyFile => {
                if let Some(reader) = self.current().and_then(|v| v.reader.as_ref()) {
                    return iced::clipboard::write(reader.content.text());
                }
            }
            Message::Link(url) => {
                if url.starts_with("https://") || url.starts_with("http://") {
                    terminal::open_link(url);
                } else if url.starts_with('/') {
                    return self.conversation_file_link(url);
                }
                return Task::none();
            }
            Message::Selecting(held) => self.selecting = held,
            Message::DemoTick => return self.demo_tick(),
            Message::Tick => self.now = Instant::now(),
            Message::NetworkPoll => return self.poll_network(),
            Message::FilesPoll => {
                if !self.backend_shown() {
                    if let Some(id) = self.selected.clone() {
                        return self.files_refresh(&id);
                    }
                }
            }
            Message::Procs(event) => return self.processes_event(event),
            Message::Perms(event) => return self.permissions_event(event),
            Message::HostControl(event) => return self.host_control_event(event),
            Message::Box(event) => return self.box_event(event),
            Message::Boxes(event) => return self.boxes_event(event),
            Message::Desktop(event) => return self.desktop_event(event),
            Message::NetworkRead(id, result) => self.network.read(id, result),
            Message::NetworkAnswer(id, host, allow, quiet) => return self.answer_network(id, host, allow, quiet),
            Message::NetworkDone(id, choice, outcome) => {
                return self.network_done(id, choice, outcome);
            }
            Message::NetworkNoticeExpired(id, serial) => self.network.expire(&id, serial),
            Message::Login(session, event) => return self.login_event(session, event),
            Message::ResizeWindow(direction) => {
                // Shelf mode is locked to its edge: only its right edge is the person's
                if self.layout.mode == layout::Mode::Shelf
                    && !matches!(direction, iced::window::Direction::East)
                {
                    return Task::none();
                }
                return iced::window::latest()
                    .and_then(move |id| iced::window::drag_resize(id, direction));
            }
            Message::GeometryTick => {
                if let Some(xid) = self.xid {
                    return blocking(
                        move || desktop::snapshot(xid),
                        |read| Message::Geometry(Read::Poll, read),
                    );
                }
            }
            Message::Geometry(why, read) => return self.geometry_read(why, read),
            Message::Placed(placed) => {
                if let Some((displays, target)) = placed {
                    self.geometry
                        .placed(displays, target.map(|t| t.rect), Instant::now());
                }
            }
            Message::SwitchPlaced(placed) => {
                if let Some((displays, target)) = placed {
                    self.geometry
                        .placed(displays, target.map(|t| t.rect), Instant::now());
                }
                // the mode moved on while this was placed: placed again, for where it is now
                if self.switching.take().is_some_and(|placed_for| placed_for != self.layout.mode) {
                    return self.place_switch();
                }
            }
            Message::Moved => self.geometry.stir(Instant::now()),
            Message::DragWindow => {
                if self.layout.mode == layout::Mode::Shelf {
                    return Task::none();
                }
                return iced::window::latest().and_then(iced::window::drag);
            }
            // the window goes; the daemon and its sessions go on (the tray icon and the hotkey
            // will bring the window back, docs/sill-gui.md)
            // Close and Ctrl+Q hide the window: it stays, so the hotkey and `sill toggle` bring
            // it back (the demo, with neither, quits)
            Message::Quit => {
                if self.demo || self.window_id.is_none() {
                    self.flush_geometry(self.layout.mode, None);
                    self.save_settings();
                    return iced::exit();
                }
                return self.hide();
            }
            Message::Minimize => {
                self.menu = None;
                return iced::window::latest().and_then(|id| iced::window::minimize(id, true));
            }
            Message::Zoom => {
                self.menu = None;
                return iced::window::latest().and_then(iced::window::toggle_maximize);
            }
            Message::ToggleTerminal => {
                return self.selected_event(session::Event::ToggleTerminal);
            }
            Message::ToggleHostTerminal => {
                return self.selected_event(session::Event::ToggleHostTerminal);
            }
            Message::Terminal(session, generation, event) => {
                // a login's terminal closing lets the session go on (`login_ui`)
                let closed = match &event {
                    terminal::Event::Ended => Some(None),
                    terminal::Event::Failed(why) => Some(Some(why.clone())),
                    _ => None,
                };
                if let Some(view) = self.views.get_mut(&session) {
                    if generation == view.terminal_generation {
                        if let Some(pane) = &mut view.terminal {
                            pane.accept(event);
                        }
                    } else if let Some((pane, parked_generation)) = &mut view.parked_terminal {
                        if generation == *parked_generation {
                            pane.accept(event);
                        }
                    }
                }
                if let Some(failed) = closed {
                    return self.login_terminal_closed(&session, generation, failed);
                }
            }
            Message::TermPaste(session, generation, text) => {
                if let Some(view) = self.current_mut()
                    && view.id == session
                    && view.terminal_generation == generation
                    && let Some(pane) = &mut view.terminal
                    && let Some(text) = text
                {
                    pane.paste(&text);
                }
            }
            Message::Files(action) => return self.files_action(action),
            Message::Resized(size) => {
                self.width = size.width;
                self.height = size.height;
                self.geometry.stir(Instant::now());
            }
            Message::ActivityHover(on) => self.activity_hovered = on,
            Message::Opened(id) => {
                self.window_id = Some(id);
                #[cfg(target_os = "macos")]
                let dress = iced::window::run(id, macos::dress).discard();
                #[cfg(not(target_os = "macos"))]
                let dress = Task::none();
                return Task::batch([
                    dress,
                    iced::window::monitor_size(id).map(Message::Monitor),
                    iced::window::run(id, x11_window).map(Message::XId),
                ]);
            }
            Message::XId(xid) => {
                self.xid = xid;
                self.no_x11 = xid.is_none();
                if self.visible {
                    return self.place();
                }
            }
            Message::Control(command) => match command {
                control::Command::Show => return self.show(0),
                control::Command::Hide => return self.hide(),
                control::Command::Toggle(time) => return self.toggle(time),
                control::Command::Quit => {
                    self.flush_geometry(self.layout.mode, None);
                    self.save_settings();
                    let _ = self.hide();
                    return iced::exit();
                }
            },
            Message::Hotkey(hotkey::Report::Pressed(time)) => return self.toggle(time),
            Message::Hotkey(hotkey::Report::Status(status)) => {
                eprintln!("sill-gui: hotkey: {status}");
                self.hotkey_status = Some(status);
            }
            Message::ToggleDecided(active, time) => {
                return if active { self.hide() } else { self.show(time) };
            }
            Message::CloseRequested => return self.hide(),
            #[cfg(target_os = "macos")]
            Message::TrayTick => {
                if let Some(tray) = &self.tray {
                    tray.tick();
                }
            }
            Message::Tray(tray::Asked::Toggle) => return self.toggle(0),
            Message::Tray(tray::Asked::Quit) => {
                if let Some(tray) = self.tray.take() {
                    tray.shutdown();
                }
                self.flush_geometry(self.layout.mode, None);
                self.save_settings();
                // the window's own VM goes with it, the guest asked to shut down first
                #[cfg(target_os = "macos")]
                return self.hide().chain(blocking(local_vm::shut_down, |_| Message::Nothing)).chain(iced::exit());
                #[cfg(not(target_os = "macos"))]
                return self.hide().chain(iced::exit());
            }
            Message::Nothing => {}
            Message::Monitor(size) => {
                self.monitor = size;
                return self.fit_without_x11(false);
            }
            Message::ScaleStep(step) => {
                return self.set_scale(settings::step_scale(self.settings.scale(), step));
            }
            Message::Layout(action) => match action {
                layout::Action::Mode(_)
                | layout::Action::ToggleShelf
                | layout::Action::ToggleFull => {
                    // a plain window (the Mac) has no modes: their keys show and put away
                    // the drawer
                    if self.layout.plain {
                        if matches!(action, layout::Action::Mode(_)) {
                            return Task::none();
                        }
                        return self.update(Message::Files(files::Action::ToggleDrawer));
                    }
                    // the person's own choice of mode: following changes leaves it be
                    self.drawer.chosen();
                    let mode = self.layout.after(action);
                    return self.set_mode(mode);
                }
                layout::Action::DragStart => {
                    self.layout.dragging = true;
                    self.layout.drag_from = Some(self.layout.drawer_width());
                }
                layout::Action::DragTo(x) => {
                    if self.layout.dragging {
                        self.layout.drag_to(x, self.drawer.shown, self.width);
                    }
                }
                layout::Action::DragEnd => {
                    self.layout.drag_from = None;
                    if std::mem::take(&mut self.layout.dragging) {
                        self.save_settings();
                    }
                }
            },
            Message::Focus(forward) => return keys::step(forward),
            Message::NewSession => {
                let agent = self
                    .tabs
                    .iter()
                    .find(|tab| Some(&tab.id) == self.selected.as_ref())
                    .map(|tab| tab.agent.clone());
                return match agent {
                    Some(agent) => self.menu_event(menu::Event::New {
                        agent,
                        ghost: false,
                        workspaces: None,
                    }),
                    None if self.menu.is_some() => Task::none(),
                    None => self.menu_event(menu::Event::Toggle),
                };
            }
            Message::CloseCurrent => {
                return match self.selected.clone() {
                    Some(id) => self.update(Message::CloseTab(id)),
                    None => Task::none(),
                };
            }
            Message::SelectTab(at) => {
                return match self.shown_tabs().get(at).map(|t| t.id.clone()) {
                    Some(id) => self.choose(id),
                    None => Task::none(),
                };
            }
            Message::StepTab(delta) => {
                let shown = self.shown_tabs();
                let Some(at) = self
                    .selected
                    .as_ref()
                    .and_then(|id| shown.iter().position(|t| &t.id == id))
                    .or((!shown.is_empty()).then_some(0))
                else {
                    return Task::none();
                };
                let count = shown.len() as i32;
                let next = (at as i32 + delta).rem_euclid(count) as usize;
                let id = shown[next].id.clone();
                return self.choose(id);
            }
            Message::Escape => return self.escape(),
            Message::Menu(event) => return self.menu_event(event),
            // the herring: the pages, at the one last shown, or back to the conversation
            Message::ToggleChanges => {
                if self.changes.take().is_some()
                    | self.settings_page.take().is_some()
                    | std::mem::take(&mut self.agents_page.shown)
                {
                    return self.restore_scroll();
                }
                return self.show_page(self.last_page);
            }
            Message::ShowPage(page) => return self.show_page(page),
            Message::StartAgents(agents) => {
                self.start_agents_pending = false;
                let ids = agents.as_ref().map(|a| a.iter().map(|d| d.id.clone()).collect()).unwrap_or_default();
                self.start_agents = Some(agents);
                return self.agent_icons_load(ids);
            }
            Message::Agents(event) => {
                // what is installed or enabled may have changed: the empty window offers it
                let changed = matches!(event, agents_ui::Event::Loaded(_)) && self.tabs.is_empty() && !self.demo;
                let task = self.agents_event(event);
                return if changed { Task::batch([task, self.start_agents_load()]) } else { task };
            }
            Message::AgentIcon(id, bytes) => {
                if let Some(bytes) = bytes {
                    self.agent_icons.insert(id, bytes);
                }
            }
            Message::Settings(event) => return self.settings_event(event),
            Message::Vm(event) => return self.vm_event(event),
            Message::SettingsReply(token, event) => {
                if self
                    .settings_page
                    .as_ref()
                    .is_some_and(|p| p.token == token)
                {
                    return self.settings_event(event);
                }
            }
            Message::DaemonSettings(revision, Ok(settings)) => {
                self.settings_pending = false;
                if revision != self.settings_revision {
                    return Task::none();
                }
                style::set_theme(&settings.theme);
                if let Some(hotkey) = &self.hotkey {
                    // `SILL_GUI_HOTKEY` overrides the shared setting: a second sill-gui beside the
                    // running one, for trying a build
                    let spec = std::env::var("SILL_GUI_HOTKEY").ok();
                    hotkey.set(
                        spec.as_deref()
                            .or(settings.hotkey.as_deref())
                            .unwrap_or(hotkey::DEFAULT),
                    );
                }
                // the tray icon as the shared setting says (unset: shown)
                let want = settings.tray.unwrap_or(true);
                if want && self.tray.is_none() {
                    if let Some(asked) = self.tray_asked.clone() {
                        self.tray = tray::start(asked);
                        if let Some(tray) = &self.tray {
                            tray.set_busy(self.tray_busy);
                        }
                    }
                } else if !want && let Some(tray) = self.tray.take() {
                    tray.shutdown();
                }
                self.daemon_settings = Some(settings);
                self.sync_drawer_follow();
            }
            Message::DaemonSettings(_, Err(_)) => self.settings_pending = false,
            Message::Changes(event) => return self.changes_event(event),
            Message::Workspaces(event) => return self.workspaces_event(event),
            Message::Crosscheck(event) => return self.crosscheck_event(event),
            Message::CloseTab(id) => {
                let ghost = self
                    .all_sessions
                    .iter()
                    .any(|s| s.session_id == id && s.ghost);
                if self.demo {
                    return Task::none();
                }
                if ghost && !self.armed_close.press(id.clone()) {
                    return Task::none();
                }
                self.armed_close.clear();
                let (method, params) = sill_client::sessions::end(&id);
                return self.act(&id, Act::End(id.clone()), method, params);
            }
            Message::Page(down) => {
                let reading = self.current().is_some_and(|v| v.files.reader.is_some());
                let ids: &[&'static str] = if reading {
                    &["markdown-reader", "diff-reader"]
                } else {
                    &["conversation"]
                };
                return keys::page(ids, down);
            }
            Message::Find(event) => return self.find_event(event),
            Message::Noop => {}
        }
        Task::none()
    }

    /// A page of the herring's in the conversation's place.
    fn show_page(&mut self, page: Page) -> Task<Message> {
        self.last_page = page;
        self.menu = None;
        self.agents_page.shown = page == Page::Agents;
        match page {
            Page::Agents => {
                self.agents_page.auto_fetch_attempted = false;
                self.changes = None;
                self.settings_page = None;
                self.agents_load(false)
            }
            Page::Changes => {
                self.settings_page = None;
                if self.changes.is_some() {
                    return Task::none();
                }
                self.changes = Some(changes_ui::ChangesPage {
                    loading: true,
                    ..changes_ui::ChangesPage::default()
                });
                self.changes_load()
            }
            Page::Settings => {
                self.changes = None;
                self.settings_page = Some(settings_ui::SettingsPage {
                    category: self.settings.settings_category,
                    ..settings_ui::SettingsPage::default()
                });
                Task::batch([
                    self.settings_load(),
                    self.vm_event(vm_ui::Event::Read),
                    self.boxes_event(boxes_ui::Event::Reload),
                    self.crosscheck_reviewers_load(),
                ])
            }
        }
    }

    /// Escape goes between the session and the backend; an
    /// open menu or file closes first, and the session's shell keeps Escape for itself.
    fn escape(&mut self) -> Task<Message> {
        if self.current().is_some_and(|v| v.image_viewer.is_some()) {
            return self.media_event(media::Event::Close);
        }
        if self.name_editor.take().is_some() {
            return Task::none();
        }
        // a popup goes first
        if self.menu.take().is_some() {
            return Task::none();
        }
        if let Some(view) = self.current_mut()
            && std::mem::take(&mut view.permissions.menu)
        {
            return Task::none();
        }
        if !self.backend_shown() && self.current().is_some_and(|v| v.annotation.is_some()) {
            return self.annotation_event(annotation::Event::Cancel);
        }
        self.stash_details();
        if self.details.take().is_some() {
            return self.restore_scroll();
        }
        // the backend's page: back to the session
        if self.changes.take().is_some()
            | self.settings_page.take().is_some()
            | std::mem::take(&mut self.agents_page.shown)
        {
            return self.restore_scroll();
        }
        if self.find.shown && !self.terminal_focused() {
            return self.find_event(find::Event::Close);
        }
        // the session's shell has the keys, Escape included (an editor in it needs it)
        if self.current().is_some_and(|v| v.terminal_shown) {
            return Task::none();
        }
        // Closing an edited source goes through the same unsaved notice as its close button.
        if self.current().is_some_and(|v| v.files.reader.is_some()) {
            return self.update(Message::Files(files::Action::CloseViewer));
        }
        // the session: to the backend's default page — the drawer and the mode stay as they are
        // (they have their own controls)
        self.armed_close.clear();
        self.show_page(self.default_page())
    }

    /// The page Escape opens from the conversation: Settings → Default page (the daemon's
    /// `default_page` setting; Changes unless set).
    fn default_page(&self) -> Page {
        match self
            .daemon_settings
            .as_ref()
            .and_then(|s| s.default_page.as_deref())
        {
            Some("agents") => Page::Agents,
            Some("settings") => Page::Settings,
            _ => Page::Changes,
        }
    }

    fn lane_update(&mut self, session: String, epoch: u64, update: lane::Update) -> Task<Message> {
        let selected = self.selected.as_deref() == Some(session.as_str());
        let selecting = self.selecting;
        let Some(view) = self
            .views
            .get_mut(&session)
            .filter(|v| v.lane_epoch == epoch)
        else {
            return Task::none();
        };
        if !matches!(update, lane::Update::Event(_)) {
            trace(|| match &update {
                lane::Update::Snapshot(c) => {
                    format!(
                        "{session} lane {epoch}: snapshot of {} items at {}",
                        c.items().len(),
                        c.seq
                    )
                }
                other => format!("{session} lane {epoch}: {other:?}"),
            });
        }
        let connection_lost = matches!(&update, lane::Update::Connecting { error: Some(_), .. });
        let was_busy = view.conversation.busy();
        let was_ended = view.ended();
        // a session already known to be summarizing, or not known at all (its first snapshot):
        // either way, no step into a summary to follow
        let was_summarizing = view.conversation.session.is_none() || view.summarizing();
        view.status.follow(&update);
        let mut grew = false;
        let mut relist = false;
        let mut checks = vec![];
        match update {
            lane::Update::Snapshot(conversation) => {
                view.conversation = *conversation;
                view.rebuild();
                view.outbox.reconcile(&view.conversation);
                view.answering.clear();
                view.failed = None;
                if selected {
                    view.seen();
                }
                grew = true;
            }
            lane::Update::Resubscribed { subscription_id } => {
                view.conversation.subscription_id = subscription_id;
            }
            lane::Update::Event(event) => {
                grew = view.accept(*event);
                relist = view.ended();
            }
            // prompts whose fate a lost connection hid: ask the daemon now
            lane::Update::Live => {
                for id in view.outbox.uncertain() {
                    if let Some(key) = view.outbox.check(&id) {
                        checks.push((id, key));
                    }
                }
            }
            lane::Update::NotRunning | lane::Update::Stopped(_) => view.lane = None,
            lane::Update::Gone(_) => {
                view.lane = None;
                relist = true;
            }
            lane::Update::Connecting { .. } => {}
        }
        let finished = was_busy && !view.conversation.busy();
        let began_summary = !was_summarizing && view.summarizing();
        let ended_now = !was_ended && view.ended();
        let followed_from = view.scroll_offset;
        let find_task = if grew && selected {
            self.recompute_find(true)
        } else {
            Task::none()
        };
        let snap = grew
            && selected
            && !selecting
            && self
                .views
                .get(&session)
                .is_some_and(|v| v.follow_tail && v.conversation_shown());
        let mut tasks: Vec<Task<Message>> = checks
            .into_iter()
            .map(|(id, key)| {
                self.act(
                    &session,
                    Act::Check(id),
                    "_sill/submissions.get",
                    json!(key),
                )
            })
            .collect();
        tasks.push(find_task);
        if connection_lost {
            if self.daemon.problem.is_none() && !self.daemon.restarting {
                self.daemon.problem =
                    Some("Checking daemon after a session connection interruption…".into());
            }
            tasks.push(self.daemon_event(daemon_ui::Event::Check));
        }
        if finished {
            if self.details.as_ref().is_some_and(|p| p.id == session) {
                tasks.push(self.details_load(session.clone()));
            }
            tasks.push(self.files_refresh(&session));
            tasks.push(self.workspaces_turn_ended(&session));
        }
        tasks.push(self.load_media(&session));
        // The selected session began its summary: its tab folds to its glyph, and the person
        // goes back to the session visited before it, if there is one.
        if began_summary && selected && !self.backend_shown() {
            tasks.push(self.leave_summary(&session));
        }
        if relist {
            tasks.push(self.list());
        }
        if snap {
            tasks.push(keys::follow_end("conversation", followed_from));
        }
        // last: the view goes with the tab
        if ended_now {
            tasks.push(self.close_ended(&session));
        }
        Task::batch(tasks)
    }

    /// A registry agent's icon for the menu and the tabs: the daemon's cached SVG when it has
    /// arrived, else the built-in glyph (built-ins always).
    pub fn agent_icon(&self, id: &str) -> iced::widget::svg::Handle {
        match self.agent_icons.get(id) {
            Some(bytes) => iced::widget::svg::Handle::from_memory(bytes.clone()),
            None => icons::for_agent(id),
        }
    }

    /// Fetch the registry icons of the non-built-in agents among `ids` that are not known yet,
    /// one connection for the batch; each arrives as `Message::AgentIcon`.
    fn agent_icons_load(&mut self, ids: Vec<String>) -> Task<Message> {
        let wanted: Vec<String> = ids
            .into_iter()
            .filter(|id| !icons::is_builtin_agent(id) && !self.agent_icons.contains_key(id) && !self.agent_icons_queued.contains(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if wanted.is_empty() {
            return Task::none();
        }
        let Some(connect) = self.connect.clone() else {
            return Task::none();
        };
        self.agent_icons_queued.extend(wanted.iter().cloned());
        let (tx, rx) = async_channel::bounded(16);
        std::thread::spawn(move || {
            let Ok(client) = connect() else {
                return;
            };
            for id in wanted {
                let bytes = (|| -> anyhow::Result<Option<Vec<u8>>> {
                    let installed: sill_proto::agent_admin::Installation = client.call("_sill/registry.installed", sill_proto::agent_admin::AgentKey { agent_id: id.clone() })?;
                    let Some(installed) = installed.installed else { return Ok(None) };
                    sill_client::agents::icon(&client, sill_proto::agent_admin::EntryKey { entry_id: id.clone(), version: installed.version, source: installed.source })
                })()
                .ok()
                .flatten();
                if tx.send_blocking(Message::AgentIcon(id, bytes)).is_err() {
                    break;
                }
            }
        });
        Task::run(
            iced::futures::stream::unfold(rx, |rx| async move { rx.recv().await.ok().map(|m| (m, rx)) }),
            |m| m,
        )
    }

    /// Read the agents a session can start with, for the window with no open session.
    pub(crate) fn start_agents_load(&mut self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            self.start_agents = Some(Ok(menu_page::demo_agents()));
            return Task::none();
        };
        if std::mem::replace(&mut self.start_agents_pending, true) {
            return Task::none();
        }
        blocking(
            move || actions.reading(sill_client::sessions::startable).map_err(|e| format!("{e:#}")),
            Message::StartAgents,
        )
    }

    fn listed(&mut self, result: Result<Vec<Session>, String>) -> Task<Message> {
        self.list_pending = false;
        if std::mem::take(&mut self.list_again) {
            // A mutation completed after this read began. Do not let its old list remove a
            // reopened view (and its drafts/resources) before the fresh list arrives.
            return self.list();
        }
        let sessions = match result {
            Ok(sessions) => sessions,
            Err(error) => {
                self.list_error = Some(error);
                return Task::none();
            }
        };
        self.list_error = None;
        self.listed = true;
        self.all_sessions = sessions.clone();
        let now = std::time::Instant::now();
        for session in &sessions {
            if let Some(view) = self.views.get_mut(&session.session_id) {
                view.host_control.saw(session.host_control.as_ref(), now);
            }
        }
        let icons = self.agent_icons_load(sessions.iter().map(|s| s.agent.clone()).collect::<Vec<_>>());
        // Refreshes may arrive newest-first or in activity order. Existing tab positions are
        // stable; newly discovered/reopened sessions append, oldest first within the batch.
        let positions: BTreeMap<String, usize> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| (tab.id.clone(), index))
            .collect();
        let created: BTreeMap<&str, u64> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.created_at.parse().unwrap_or(0)))
            .collect();
        let drafts: Vec<_> = self
            .tabs
            .iter()
            .filter(|tab| self.unsaved_source(&tab.id))
            .cloned()
            .collect();
        self.tabs = sessions
            .iter()
            .filter(|s| s.ended_at.is_none())
            .map(Tab::from)
            .collect();
        // An externally ended session retains its tab while it holds unsaved source edits.
        for tab in drafts {
            if !self.tabs.iter().any(|t| t.id == tab.id) {
                self.tabs.push(tab);
            }
        }
        self.tabs.sort_by(|a, b| {
            positions
                .get(&a.id)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&positions.get(&b.id).copied().unwrap_or(usize::MAX))
                .then_with(|| created.get(a.id.as_str()).cmp(&created.get(b.id.as_str())))
                .then_with(|| a.id.cmp(&b.id))
        });
        self.prune_closed();
        // the window with no open session offers the agents: read them as it empties, and again
        // after a failed read
        let start = if self.tabs.is_empty() && (!positions.is_empty() || !matches!(self.start_agents, Some(Ok(_)))) {
            self.start_agents_load()
        } else {
            Task::none()
        };
        // A stopped jail has no live processes or exposed ports, even while its tab (or an
        // externally ended session's unsaved source) remains open. Invalidate in-flight polls.
        for (id, view) in &mut self.views {
            if !sessions
                .iter()
                .any(|s| s.session_id == *id && s.running && s.ended_at.is_none())
            {
                view.processes.read(vec![], None);
            }
        }
        // watch every running session for supervision; watching never starts one
        let watch: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| t.running)
            .filter(|t| {
                self.views
                    .get(&t.id)
                    .is_none_or(|v| matches!(v.status, Status::Closed | Status::NotRunning))
            })
            .map(|t| t.id.clone())
            .collect();
        for id in watch {
            self.open_lane(&id, lane::Mode::Watch);
        }
        let mut tasks = vec![start];
        if let Some(key) = self.initial.take() {
            let found: Vec<&Session> = sessions
                .iter()
                .filter(|s| s.ended_at.is_none())
                .filter(|s| s.session_id == key || s.session_id.starts_with(&key) || s.title == key)
                .collect();
            let exact = found.iter().find(|s| s.session_id == key);
            match (exact, found.as_slice()) {
                (Some(s), _) | (None, [s]) => {
                    let id = s.session_id.clone();
                    tasks.push(self.choose(id));
                }
                (None, []) => self.list_error = Some(format!("no open session {key}")),
                (None, _) => self.list_error = Some(format!("ambiguous session {key}")),
            }
        }
        if let Some(id) = self
            .pending_choose
            .take_if(|id| self.tabs.iter().any(|t| &t.id == id))
        {
            let choose = self.choose(id);
            tasks.push(choose.chain(self.focus_prompt()));
        }
        tasks.push(self.reselect());
        tasks.push(icons);
        Task::batch(tasks)
    }

    /// The session holds source edits not yet saved: its tab stays when it ends.
    fn unsaved_source(&self, id: &str) -> bool {
        self.views
            .get(id)
            .and_then(|v| v.reader.as_ref())
            .is_some_and(|r| r.dirty || r.saving.is_some())
    }

    /// After `tabs` lost a session that ended or went: its view goes, and with it its lane and
    /// terminal; it leaves the visiting order, and the selection when it was the one selected.
    fn prune_closed(&mut self) {
        let open: BTreeSet<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        self.views.retain(|id, _| open.contains(id.as_str()));
        self.tab_history.retain(|id| open.contains(id.as_str()));
        if self
            .selected
            .as_ref()
            .is_some_and(|s| !open.contains(s.as_str()))
        {
            self.selected = None;
        }
    }

    /// With nothing selected: the tab visited last, else the first running one.
    fn reselect(&mut self) -> Task<Message> {
        if self.selected.is_some() {
            return Task::none();
        }
        let next = self
            .tab_history
            .last()
            .cloned()
            .or_else(|| self.tabs.iter().find(|t| t.running).map(|t| t.id.clone()));
        match next {
            Some(id) => self.choose(id),
            None => Task::none(),
        }
    }

    /// The session's lane said it ended: its tab closes now, as `listed` would close it once the
    /// list it asks for returned — until then the tab would stand there, unfolded after a closing
    /// summary. Unsaved source edits keep the tab, as in `listed`.
    fn close_ended(&mut self, id: &str) -> Task<Message> {
        if self.unsaved_source(id) {
            return Task::none();
        }
        self.tabs.retain(|t| t.id != id);
        self.prune_closed();
        self.reselect()
    }

    /// The tabs in the order the bar shows them: summarizing sessions first, folded to their
    /// glyphs, then the rest; each group keeps the tabs' own order, which a summary leaves alone,
    /// so a tab goes back to its place when its summary ends. Numbered and stepped selection
    /// follow this order.
    pub fn shown_tabs(&self) -> Vec<&Tab> {
        let folded = |t: &&Tab| self.views.get(&t.id).is_some_and(SessionView::summarizing);
        let (mut shown, rest): (Vec<&Tab>, Vec<&Tab>) = self.tabs.iter().partition(folded);
        shown.extend(rest);
        shown
    }

    /// Away from `session`, which began its summary: to the most recently visited other open
    /// tab that is not summarizing too. With none, the selection stays.
    fn leave_summary(&mut self, session: &str) -> Task<Message> {
        let back = self
            .tab_history
            .iter()
            .rev()
            .filter(|id| id.as_str() != session)
            .find(|id| {
                self.tabs.iter().any(|t| &t.id == *id)
                    && !self.views.get(*id).is_some_and(SessionView::summarizing)
            })
            .cloned();
        match back {
            Some(id) => self.choose(id),
            None => Task::none(),
        }
    }

    fn choose(&mut self, id: String) -> Task<Message> {
        let details_open = self.details.is_some();
        self.stash_details();
        self.details = None;
        if !self.tabs.iter().any(|t| t.id == id) {
            return Task::none();
        }
        let again = self.selected.as_deref() == Some(id.as_str())
            && self.changes.is_none()
            && !details_open;
        self.changes = None;
        self.settings_page = None;
        self.agents_page.shown = false;
        self.tab_history.retain(|visited| visited != &id);
        self.tab_history.push(id.clone());
        self.selected = Some(id.clone());
        let view = self
            .views
            .entry(id.clone())
            .or_insert_with(|| SessionView::new(&id));
        view.seen();
        // the person opened it: attaching may start it now
        if let Some(lane) = &view.lane {
            lane.promote();
        }
        let needs_lane = view.lane.is_none() && !self.demo;
        let load_files = view.files.roots_pending.is_none() && !view.files.initialized;
        if needs_lane {
            self.open_lane(&id, lane::Mode::Open);
        }
        if again {
            return Task::none();
        }
        let mut tasks = vec![self.restore_scroll(), self.read_permissions(&id)];
        if load_files {
            tasks.push(self.files_refresh(&id));
        }
        // its changes opened the drawer and went while another session was shown
        tasks.push(self.follow_return(&id));
        Task::batch(tasks)
    }

    fn done(&mut self, session: String, act: Act, outcome: Outcome) -> Task<Message> {
        trace(|| format!("{session} {act:?} → {outcome:?}"));
        if let Act::Changes(verb) = &act {
            if let Some(page) = &mut self.changes {
                page.said = Some(match &outcome {
                    Outcome::Done(value) => sill_client::changes::said(verb, value),
                    other => format!("{verb}: {}", other.problem().unwrap_or_default()),
                });
                page.loading = true;
            }
            return self.changes_load();
        }
        let lifecycle = match &act {
            Act::Create => Some("Starting the session"),
            Act::Reopen(_) => Some("Reopening the session"),
            Act::End(_) => Some("Closing the session"),
            Act::Delete => Some("Deleting the session"),
            _ => None,
        };
        if let Some(what) = lifecycle {
            match (&act, &outcome) {
                (Act::Create, Outcome::Done(value)) => {
                    self.pending_choose = sill_client::sessions::created(value);
                }
                (Act::Reopen(id), Outcome::Done(_)) => self.pending_choose = Some(id.clone()),
                (_, Outcome::Done(_)) => {}
                (_, other) => {
                    let problem = format!("{what}: {}", other.problem().unwrap_or_default());
                    match self.current_mut() {
                        Some(view) => view.failed = Some(problem),
                        None => self.list_error = Some(problem),
                    }
                }
            }
            return self.list();
        }
        let Some(view) = self.views.get_mut(&session) else {
            return Task::none();
        };
        let settled = match &act {
            Act::Send(id) => Some((id.clone(), view.outbox.settle(id, &outcome))),
            Act::Check(id) => Some((id.clone(), view.outbox.checked(id, &outcome))),
            Act::Answer(question) => {
                view.answering.remove(question);
                None
            }
            _ => None,
        };
        match settled {
            // it never reached the agent: the text goes back where it was typed, if that is free
            Some((id, Settled::Kept))
                if view.draft.text().trim().is_empty() && view.attachments.is_empty() =>
            {
                if let Some(pending) = view.outbox.take(&id) {
                    let reason = match &pending.state {
                        sill_client::outbox::State::Failed(reason) => reason.clone(),
                        _ => String::new(),
                    };
                    // an Apply's request to commit that never reached the agent is not waited for
                    view.workspaces.committing = None;
                    view.draft = editor::Editor::with_text(pending.text());
                    view.attachments = view.pending_attachments.remove(&id).unwrap_or_else(|| pending.submit.attachments.into_iter().map(Into::into).collect());
                    view.failed = Some(format!(
                        "Not sent: {reason}. The prompt is back in the composer."
                    ));
                    if self.selected.as_deref() == Some(session.as_str()) {
                        return operation::focus("prompt");
                    }
                }
            }
            Some(_) => {}
            None => {
                if let Some(problem) = outcome.problem() {
                    let what = match act {
                        Act::Resume => "Sending the held prompt",
                        Act::Create
                        | Act::Reopen(_)
                        | Act::End(_)
                        | Act::Delete
                        | Act::Changes(_) => unreachable!(),
                        Act::Stop => "Stopping the turn",
                        Act::Answer(_) => "Answering the question",
                        Act::Unqueue => "Removing the queued prompt",
                        Act::SendNow => "Sending the queued prompt now",
                        Act::Login => "Logging in",
                        Act::CarryOn => "Telling the agent to carry on",
                        Act::Control => "Changing the agent's setting",
                        Act::Send(_) | Act::Check(_) => unreachable!(),
                    };
                    let uncertain = matches!(outcome, Outcome::Uncertain(_));
                    view.failed = Some(if uncertain {
                        format!(
                            "{what}: the connection was lost ({problem}); it may have taken effect — the conversation shows what did."
                        )
                    } else {
                        format!("{what}: {problem}")
                    });
                }
            }
        }
        Task::none()
    }

    fn demo_tick(&mut self) -> Task<Message> {
        let step = self.demo_step;
        self.demo_step += 1;
        let Some(view) = self.views.get_mut("demo") else {
            return Task::none();
        };
        if let Some(chunk) = demo::CHUNKS.get(step) {
            view.accept(demo::append(view.conversation.seq + 1, chunk));
        } else if step == demo::CHUNKS.len() {
            for event in demo::waiting(view.conversation.seq + 1) {
                view.accept(event);
            }
        }
        let scroll_offset = view.scroll_offset;
        let find_task = self.recompute_find(true);
        let snap = !self.selecting
            && self
                .views
                .get("demo")
                .is_some_and(|v| v.follow_tail && v.conversation_shown());
        if snap {
            Task::batch([find_task, keys::follow_end("conversation", scroll_offset)])
        } else {
            find_task
        }
    }
}

/// With `SILL_GUI_TRACE` set, what happened, on stderr: requests' outcomes, lanes' states.
fn trace(what: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SILL_GUI_TRACE").is_some()) {
        eprintln!("sill-gui: {}", what());
    }
}
