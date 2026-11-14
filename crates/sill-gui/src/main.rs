mod changes_ui;
mod demo;
mod fonts;
mod forms;
mod icons;
mod keys;
mod layout;
mod menu;
#[cfg(test)]
mod perf;
mod selection;
mod settings;
#[cfg(test)]
mod snapshots;
mod style;
mod terminal;
#[cfg(test)]
mod tests;
mod ui;
mod viewer;
mod workspace;
mod workspace_ui;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use iced::widget::{markdown, operation, scrollable, text_editor};
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
use sill_proto::sfp::{
    Answer, AnswerQuestion, CancelTurn, ConfigValue, Control, SessionControl, SubmissionKey,
};

#[derive(Parser)]
#[command(about = "Experimental Iced frontend for Sill (runs beside the shelf)")]
struct Options {
    /// Open an existing session by exact ID, unique prefix or title.
    #[arg(long, conflicts_with = "demo")]
    session: Option<String>,
    /// Show a synthetic streaming conversation without connecting to a daemon.
    #[arg(long)]
    demo: bool,
}

fn main() -> iced::Result {
    prefer_low_power_gpu();
    let options = Options::parse();
    fonts::install();
    iced::application(
        move || App::new(options.demo, options.session.clone()),
        App::update,
        App::view,
    )
    .title("Sill · Iced preview")
    .theme(|_: &App| style::theme())
    .subscription(App::subscription)
    .default_font(Font::with_name(fonts::SANS_FAMILY))
    .window_size((720.0, 860.0))
    // no title bar, like the shelf: the top bar's empty space moves the window, Ctrl+Q quits
    .decorations(false)
    .run()
}

/// The shelf stays open all day: draw on the integrated GPU and never wake a discrete one, unless
/// the person asks otherwise (`WGPU_POWER_PREF=high`). iced 0.14 asks wgpu for the
/// high-performance adapter, and this variable is the only way to change that.
fn prefer_low_power_gpu() {
    if std::env::var_os("WGPU_POWER_PREF").is_none() {
        // SAFETY: the first thing main does, before any thread exists that could read the
        // environment.
        unsafe { std::env::set_var("WGPU_POWER_PREF", "low") };
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
    Control,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// An update from a session's lane: the session, the lane it came from, the update.
    Lane(String, u64, lane::Update),
    Listed(Result<Vec<Session>, String>),
    /// The outcome of a request the person made in a session.
    Done(String, Act, Outcome),
    Choose(String),
    Refresh,
    Copy(String),
    /// Copy the whole conversation as text, or the open file (read when pressed, not per frame).
    CopyTranscript,
    CopyFile,
    Link(String),
    /// A selection drag in the conversation started (`true`) or ended (`false`).
    Selecting(bool),
    ToggleText,
    RefreshText,
    TextAction(text_editor::Action),
    DemoTick,
    Tick,
    Scrolled(bool, f32),
    Latest,
    Prompt(text_editor::Action),
    Send,
    Stop,
    Answer {
        question_id: String,
        option_id: String,
    },
    Unqueue(String),
    ToggleTool(String),
    ToggleAnswered(String),
    SetMode(Pick),
    SetOption(String, Pick),
    /// A prompt that did not arrive: back into the composer, sent again, checked, or dropped.
    EditSend(String),
    RetrySend(String),
    CheckSend(String),
    DiscardSend(String),
    /// From the status strip: back to the conversation, from files or the transcript.
    ShowConversation,
    /// A press on the top bar's empty space: the window manager moves the window.
    DragWindow,
    Quit,
    /// `>_`: the session's jail shell in place of the prompt, or the prompt again.
    ToggleTerminal,
    Terminal(String, u64, terminal::Event),
    TermAction(terminal::Action),
    TermPaste(Option<String>),
    /// End the shell itself (hiding the pane only detaches from it).
    EndTerminal,
    ReopenTerminal,
    Workspace(workspace::Action),
    /// The window's new width.
    Resized(iced::Size),
    /// The window is open: ask for its monitor's size.
    Opened(iced::window::Id),
    /// The monitor's size, once the window knows it: Shelf mode's width comes from it.
    Monitor(Option<iced::Size>),
    /// The pointer came over the top bar's activity, or left it: the timer shows meanwhile.
    ActivityHover(bool),
    /// Full conversation, and the drawer's edge dragged.
    Layout(layout::Action),
    /// Tab (true) or Shift+Tab: the keyboard moves to the next or previous control.
    Focus(bool),
    /// Ctrl+Shift+Right (1) or Left (-1): the next or previous session.
    StepTab(i32),
    /// Escape: the file or transcript closes, else the drawer, else the prompt takes the keys.
    Escape,
    /// PageDown (true) or PageUp in the conversation or the open file.
    Page(bool),
    /// The `+` menu: start, reopen, delete sessions.
    Menu(menu::Event),
    /// The herring: the Changes page, or back to the conversation.
    ToggleChanges,
    Changes(changes_ui::Event),
    /// A tab's ×: end its session (a ghost's needs a second press).
    CloseTab(String),
    /// A form question: a field edited, the form sent, or declined.
    FormEdit(String, forms::Edit),
    SubmitForm(String),
    DeclineForm(String),
    /// A held prompt: send it after all, or drop it.
    ResumeHeld(String),
    DiscardHeld(String),
    Noop,
}

pub struct Rendered {
    pub source: String,
    pub markdown: markdown::Content,
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
    pub draft: text_editor::Content,
    pub outbox: Outbox,
    /// A request the person made that did not take effect; shown until the next one.
    pub failed: Option<String>,
    pub open_tools: BTreeSet<String>,
    pub open_answers: BTreeSet<String>,
    /// Answers on their way, so a second press does not send another.
    pub answering: BTreeSet<String>,
    pub follow_tail: bool,
    pub scroll_offset: f32,
    pub text_view: Option<text_editor::Content>,
    pub terminal: Option<terminal::Pane>,
    pub terminal_shown: bool,
    terminal_generation: u64,
    pub files: FilesState,
    pub reader: Option<workspace::ReaderView>,
    /// The failed turn the person has looked at, so its mark goes.
    seen_failure: Option<String>,
    /// What the person has entered in open form questions.
    pub forms: BTreeMap<String, forms::Form>,
}

impl SessionView {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            clock_shown: std::cell::Cell::new(None),
            lane: None,
            lane_epoch: 0,
            status: Status::Closed,
            conversation: Conversation::default(),
            rendered: BTreeMap::new(),
            draft: text_editor::Content::new(),
            outbox: Outbox::default(),
            failed: None,
            open_tools: BTreeSet::new(),
            open_answers: BTreeSet::new(),
            answering: BTreeSet::new(),
            follow_tail: true,
            scroll_offset: 0.0,
            text_view: None,
            terminal: None,
            terminal_shown: false,
            terminal_generation: 0,
            files: FilesState::default(),
            reader: None,
            seen_failure: None,
            forms: BTreeMap::new(),
        }
    }

    /// The conversation is on screen (not a file, not the transcript).
    pub fn conversation_shown(&self) -> bool {
        self.text_view.is_none() && self.files.reader.is_none()
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
            Some(item) => item_text(item),
        };
        match self.rendered.get_mut(id) {
            Some(cached) if cached.source == source => {}
            Some(cached) if source.starts_with(&cached.source) => {
                cached.markdown.push_str(&source[cached.source.len()..]);
                cached.source = source;
            }
            _ => {
                self.rendered.insert(
                    id.to_string(),
                    Rendered {
                        markdown: markdown::Content::parse(&source),
                        source,
                    },
                );
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
    pub demo: bool,
    connect: Option<Connector>,
    actions: Option<Arc<Actions>>,
    /// Where every lane reports, tagged with its session and epoch.
    lanes: Option<async_channel::Sender<(String, u64, lane::Update)>>,
    next_lane: u64,
    pub tabs: Vec<Tab>,
    /// Every session the daemon has, newest first (the menu's Reopen list).
    pub all_sessions: Vec<Session>,
    pub views: BTreeMap<String, SessionView>,
    /// The `+` menu, while open.
    pub menu: Option<menu::Menu>,
    /// The Changes page, while shown in place of the conversation.
    pub changes: Option<changes_ui::ChangesPage>,
    /// A session just created or reopened: chosen once the list shows it.
    pending_choose: Option<String>,
    /// A ghost's tab whose × was pressed once.
    pub armed_close: Option<String>,
    pub selected: Option<String>,
    /// `--session`, chosen when the first list arrives.
    initial: Option<String>,
    /// Trouble reading the daemon's session list.
    pub list_error: Option<String>,
    pub listed: bool,
    pub workspace: workspace::Workspace,
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
    /// The monitor's logical size, once known.
    pub monitor: Option<iced::Size>,
    /// The pointer is over the top bar's activity (the shelf shows the timer then).
    pub activity_hovered: bool,
    /// A selection is being dragged in the conversation: following the tail would pull the text
    /// from under the pointer, so new text does not scroll it until the drag ends.
    pub selecting: bool,
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
        Self::with(demo, initial, (!demo).then(sill_client::connect))
    }

    /// The app on `connect`'s daemon (none in `--demo`).
    pub fn with(
        demo: bool,
        initial: Option<String>,
        connect: Option<Connector>,
    ) -> (Self, Task<Message>) {
        let (workspace, file_task) = workspace::Workspace::new(connect.clone());
        let mut app = Self {
            demo,
            actions: connect.clone().map(Actions::new),
            connect,
            lanes: None,
            next_lane: 0,
            tabs: Vec::new(),
            all_sessions: Vec::new(),
            views: BTreeMap::new(),
            menu: None,
            changes: None,
            pending_choose: None,
            armed_close: None,
            selected: None,
            initial,
            list_error: None,
            listed: false,
            workspace,
            now: Instant::now(),
            demo_step: 0,
            width: 720.0,
            height: 860.0,
            layout: layout::Layout::default(),
            settings: settings::Settings::default(),
            persist: false,
            monitor: None,
            activity_hovered: false,
            selecting: false,
        };
        if demo {
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
        app.workspace.shown = app.layout.mode == layout::Mode::Drawer;
        let (tx, rx) = async_channel::bounded(256);
        app.lanes = Some(tx);
        let lanes = iced::futures::stream::unfold(rx, |rx| async move {
            rx.recv().await.ok().map(|update| (update, rx))
        });
        let list = app.list();
        (
            app,
            Task::batch([
                file_task,
                Task::run(lanes, |(session, epoch, update)| {
                    Message::Lane(session, epoch, update)
                }),
                list,
            ]),
        )
    }

    fn save_settings(&mut self) {
        self.settings.mode = Some(self.layout.mode.name().into());
        self.settings.drawer_width = self.layout.drawer;
        if self.persist {
            settings::save(&self.settings);
        }
    }

    /// The working size (Drawer and Full): as the window last had it, else half the monitor.
    fn work_size(&self) -> Option<iced::Size> {
        let height = self.settings.work_height.unwrap_or(self.height);
        match self.settings.work_width {
            Some(width) => Some(iced::Size::new(width, height)),
            None => self
                .monitor
                .map(|m| iced::Size::new((m.width * layout::WORK_SHARE).round(), height)),
        }
    }

    /// The window's size rule for the mode it is in: Shelf mode is at most its share of the
    /// monitor and held there (a maximum the window manager may honour; a tiling one decides for
    /// itself); the other modes have the working size and no maximum.
    fn fit_window(&self, entering: bool) -> Task<Message> {
        // nothing to fit before the window knows its monitor (and no window in tests)
        if self.monitor.is_none() {
            return Task::none();
        }
        let mut tasks = vec![];
        if self.layout.mode == layout::Mode::Shelf {
            let Some(monitor) = self.monitor else {
                return Task::none();
            };
            let width = (monitor.width * self.settings.shelf_share()).round();
            let max = iced::Size::new(width, monitor.height.max(self.height));
            let size = iced::Size::new(width.min(self.width.max(layout::MAIN_MIN)), self.height);
            let size = if entering {
                iced::Size::new(width, self.height)
            } else {
                size
            };
            tasks.push(
                iced::window::latest()
                    .and_then(move |id| iced::window::set_max_size(id, Some(max))),
            );
            tasks.push(iced::window::latest().and_then(move |id| iced::window::resize(id, size)));
        } else {
            tasks.push(iced::window::latest().and_then(|id| iced::window::set_max_size(id, None)));
            if entering && let Some(size) = self.work_size() {
                tasks.push(
                    iced::window::latest().and_then(move |id| iced::window::resize(id, size)),
                );
            }
        }
        Task::batch(tasks)
    }

    /// Into `mode`: the drawer in Drawer mode only (a quick look elsewhere closes), the window's
    /// size from Shelf to working and back; Drawer ⇄ Full keep the window as it is.
    fn set_mode(&mut self, mode: layout::Mode) -> Task<Message> {
        use layout::Mode;
        let old = self.layout.mode;
        if old == mode {
            return Task::none();
        }
        if old != Mode::Shelf {
            self.settings.work_width = Some(self.width);
            self.settings.work_height = Some(self.height);
        }
        if mode == Mode::Full {
            self.layout.before_full = old;
        }
        self.layout.mode = mode;
        let opening = mode == Mode::Drawer && !self.workspace.shown;
        self.workspace.shown = mode == Mode::Drawer;
        self.save_settings();
        let mut tasks = vec![self.restore_scroll()];
        if opening && let Some(id) = self.selected.clone() {
            tasks.push(self.files_refresh(&id));
        }
        if (old == Mode::Shelf) != (mode == Mode::Shelf) {
            tasks.push(self.fit_window(true));
        }
        Task::batch(tasks)
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![];
        if self.demo && self.demo_step <= demo::CHUNKS.len() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(600)).map(|_| Message::DemoTick));
        }
        // while the agent is busy: the summaries' clocks and the timer's width, once a second
        // (the spinner and the timer draw themselves every frame, without a new view)
        if self.current().is_some_and(|v| v.conversation.busy()) {
            subscriptions.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        // sessions started or ended elsewhere appear and go
        if !self.demo {
            subscriptions
                .push(iced::time::every(Duration::from_secs(15)).map(|_| Message::Refresh));
        }
        subscriptions.push(iced::keyboard::listen().filter_map(keys::shortcut));
        subscriptions.push(iced::window::resize_events().map(|(_, size)| Message::Resized(size)));
        subscriptions.push(self.layout.subscription());
        // the monitor's size once the window is there: Shelf mode's width comes from it
        subscriptions.push(iced::window::open_events().map(Message::Opened));
        Subscription::batch(subscriptions)
    }

    /// The drawer is over the conversation rather than beside it.
    pub fn narrow(&self) -> bool {
        matches!(
            self.layout.drawer(self.workspace.shown, self.width),
            layout::Drawer::Over(_)
        )
    }

    pub fn current(&self) -> Option<&SessionView> {
        self.views.get(self.selected.as_deref()?)
    }

    fn current_mut(&mut self) -> Option<&mut SessionView> {
        self.views.get_mut(self.selected.as_deref()?)
    }

    /// The session list, read off the GUI thread.
    fn list(&self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
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

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Lane(session, epoch, update) => {
                return self.lane_update(session, epoch, update);
            }
            Message::Listed(result) => return self.listed(result),
            Message::Done(session, act, outcome) => return self.done(session, act, outcome),
            Message::Choose(id) => return self.choose(id),
            Message::Refresh => {
                let mut tasks = vec![self.list()];
                if self
                    .changes
                    .as_ref()
                    .is_some_and(|c| !c.history && !c.loading)
                {
                    tasks.push(self.changes_event(changes_ui::Event::Refresh));
                }
                // the pins' chips show their changed lines with the drawer closed too
                if let Some(id) = self.selected.clone() {
                    tasks.push(self.files_refresh(&id));
                }
                return Task::batch(tasks);
            }
            Message::Scrolled(at_bottom, offset) => {
                if let Some(view) = self.current_mut() {
                    view.follow_tail = at_bottom;
                    view.scroll_offset = offset;
                }
            }
            Message::Latest => {
                if let Some(view) = self.current_mut() {
                    view.follow_tail = true;
                }
                return operation::snap_to_end("conversation");
            }
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::CopyTranscript => {
                if let Some(view) = self.current() {
                    return iced::clipboard::write(view.conversation.transcript());
                }
            }
            Message::CopyFile => {
                if let Some(file) = self.current().and_then(|v| v.files.reader.as_ref()) {
                    return iced::clipboard::write(file.text.clone());
                }
            }
            Message::Link(url) => return iced::clipboard::write(url),
            Message::Selecting(held) => self.selecting = held,
            Message::ToggleText => {
                if let Some(view) = self.current_mut() {
                    view.text_view = match view.text_view {
                        Some(_) => None,
                        None => Some(text_editor::Content::with_text(
                            &view.conversation.transcript(),
                        )),
                    };
                    if view.text_view.is_none() {
                        return self.restore_scroll();
                    }
                }
            }
            Message::RefreshText => {
                if let Some(view) = self.current_mut() {
                    view.text_view = Some(text_editor::Content::with_text(
                        &view.conversation.transcript(),
                    ));
                }
            }
            Message::TextAction(action) => {
                if !matches!(action, text_editor::Action::Edit(_))
                    && let Some(content) = self.current_mut().and_then(|v| v.text_view.as_mut())
                {
                    content.perform(action);
                }
            }
            Message::DemoTick => return self.demo_tick(),
            Message::Tick => self.now = Instant::now(),
            Message::Prompt(action) => {
                if let Some(view) = self.current_mut() {
                    view.draft.perform(action);
                }
            }
            Message::Send => return self.send(),
            Message::Stop => return self.stop(),
            Message::Answer {
                question_id,
                option_id,
            } => return self.answer(question_id, option_id),
            Message::Unqueue(submission_id) => {
                let Some(view) = self.current() else {
                    return Task::none();
                };
                let params = SubmissionKey {
                    session_id: view.id.clone(),
                    submission_id,
                };
                return self.act(
                    &view.id,
                    Act::Unqueue,
                    "_sill/submissions.cancel",
                    json!(params),
                );
            }
            Message::ToggleTool(id) => {
                if let Some(view) = self.current_mut() {
                    toggle(&mut view.open_tools, id);
                }
            }
            Message::ToggleAnswered(id) => {
                if let Some(view) = self.current_mut() {
                    toggle(&mut view.open_answers, id);
                }
            }
            Message::SetMode(pick) => {
                return self.control(Control::SetMode {
                    mode_id: pick.value,
                });
            }
            Message::SetOption(option_id, pick) => {
                let value = match pick.boolean {
                    Some(value) => ConfigValue::Bool { value },
                    None => ConfigValue::Id { value: pick.value },
                };
                return self.control(Control::SetConfig { option_id, value });
            }
            Message::EditSend(id) => {
                if let Some(view) = self.current_mut()
                    && let Some(pending) = view.outbox.take(&id)
                {
                    let mut text = view.draft.text().trim_end().to_string();
                    if !text.trim().is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(pending.text());
                    view.draft = text_editor::Content::with_text(&text);
                    return operation::focus("prompt");
                }
            }
            Message::RetrySend(id) => {
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                let Some(submit) = view.outbox.retry(&id) else {
                    return Task::none();
                };
                let session = view.id.clone();
                return self.act(
                    &session,
                    Act::Send(id),
                    "_sill/sessions.submit",
                    json!(submit),
                );
            }
            Message::CheckSend(id) => {
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                let Some(key) = view.outbox.check(&id) else {
                    return Task::none();
                };
                let session = view.id.clone();
                return self.act(
                    &session,
                    Act::Check(id),
                    "_sill/submissions.get",
                    json!(key),
                );
            }
            Message::DiscardSend(id) => {
                if let Some(view) = self.current_mut() {
                    view.outbox.take(&id);
                }
            }
            Message::ShowConversation => {
                if let Some(view) = self.current_mut() {
                    view.text_view = None;
                    view.files.close_reader();
                    view.reader = None;
                }
                if self.workspace.shown && self.narrow() {
                    self.workspace.shown = false;
                }
                return self.restore_scroll();
            }
            Message::DragWindow => return iced::window::latest().and_then(iced::window::drag),
            // the window goes; the daemon and its sessions go on (the tray icon and the hotkey
            // will bring the window back, docs/sill-gui.md)
            Message::Quit => {
                if self.layout.mode != layout::Mode::Shelf {
                    self.settings.work_width = Some(self.width);
                    self.settings.work_height = Some(self.height);
                }
                self.save_settings();
                return iced::exit();
            }
            Message::ToggleTerminal => return self.toggle_terminal(),
            Message::ReopenTerminal => return self.open_terminal(),
            Message::EndTerminal => {
                if let Some(pane) = self.current().and_then(|v| v.terminal.as_ref()) {
                    pane.close();
                }
            }
            Message::Terminal(session, generation, event) => {
                if let Some(view) = self.views.get_mut(&session)
                    && generation == view.terminal_generation
                    && let Some(pane) = &mut view.terminal
                {
                    pane.accept(event);
                }
            }
            Message::TermAction(action) => {
                let Some(pane) = self.current_mut().and_then(|v| v.terminal.as_mut()) else {
                    return Task::none();
                };
                match action {
                    terminal::Action::Copy => {
                        if let Some(selected) = pane.selection() {
                            return iced::clipboard::write(selected);
                        }
                    }
                    terminal::Action::Paste => {
                        return iced::clipboard::read().map(Message::TermPaste);
                    }
                    action => pane.perform(action),
                }
            }
            Message::TermPaste(text) => {
                if let (Some(pane), Some(text)) =
                    (self.current_mut().and_then(|v| v.terminal.as_mut()), text)
                {
                    pane.paste(&text);
                }
            }
            Message::Workspace(action) => return self.workspace_action(action),
            Message::Resized(size) => {
                self.width = size.width;
                self.height = size.height;
            }
            Message::ActivityHover(on) => self.activity_hovered = on,
            Message::Opened(id) => {
                return iced::window::monitor_size(id).map(Message::Monitor);
            }
            Message::Monitor(size) => {
                self.monitor = size;
                return self.fit_window(false);
            }
            Message::Layout(action) => match action {
                layout::Action::Mode(_)
                | layout::Action::ToggleShelf
                | layout::Action::ToggleFull => {
                    let mode = self.layout.after(action);
                    return self.set_mode(mode);
                }
                layout::Action::DragStart => self.layout.dragging = true,
                layout::Action::DragTo(x) => {
                    if self.layout.dragging {
                        self.layout.drag_to(x, self.workspace.shown, self.width);
                    }
                }
                layout::Action::DragEnd => {
                    if std::mem::take(&mut self.layout.dragging) {
                        self.save_settings();
                    }
                }
            },
            Message::Focus(forward) => return keys::step(forward),
            Message::StepTab(delta) => {
                let Some(at) = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.tabs.iter().position(|t| &t.id == id))
                    .or((!self.tabs.is_empty()).then_some(0))
                else {
                    return Task::none();
                };
                let count = self.tabs.len() as i32;
                let next = (at as i32 + delta).rem_euclid(count) as usize;
                let id = self.tabs[next].id.clone();
                return self.choose(id);
            }
            Message::Escape => return self.escape(),
            Message::Menu(event) => return self.menu_event(event),
            Message::ToggleChanges => {
                if self.changes.take().is_some() {
                    return self.restore_scroll();
                }
                self.changes = Some(changes_ui::ChangesPage {
                    loading: true,
                    ..changes_ui::ChangesPage::default()
                });
                return self.changes_load();
            }
            Message::Changes(event) => return self.changes_event(event),
            Message::CloseTab(id) => {
                let ghost = self
                    .all_sessions
                    .iter()
                    .any(|s| s.session_id == id && s.ghost);
                if self.demo {
                    return Task::none();
                }
                if ghost && self.armed_close.as_deref() != Some(id.as_str()) {
                    self.armed_close = Some(id);
                    return Task::none();
                }
                self.armed_close = None;
                let (method, params) = sill_client::sessions::end(&id);
                return self.act(&id, Act::End(id.clone()), method, params);
            }
            Message::FormEdit(question_id, edit) => {
                if let Some(view) = self.current_mut()
                    && let Some(question) = view.conversation.questions.get(&question_id)
                {
                    let form = view.forms.entry(question_id).or_default();
                    forms::apply(form, question, edit);
                }
            }
            Message::SubmitForm(question_id) => {
                let Some(view) = self.current_mut() else {
                    return Task::none();
                };
                let Some(question) = view.conversation.questions.get(&question_id) else {
                    return Task::none();
                };
                let form = view.forms.get(&question_id).cloned().unwrap_or_default();
                match forms::values(&form, question) {
                    Ok(values) => return self.answer_with(question_id, Answer::Form { values }),
                    Err(problem) => {
                        view.failed = Some(format!("The form cannot be sent: {problem}"))
                    }
                }
            }
            Message::DeclineForm(question_id) => {
                return self.answer_with(question_id, Answer::Cancel);
            }
            Message::ResumeHeld(submission_id) | Message::DiscardHeld(submission_id)
                if self.demo =>
            {
                let _ = submission_id;
            }
            Message::ResumeHeld(submission_id) => {
                let Some(view) = self.current() else {
                    return Task::none();
                };
                let key = SubmissionKey {
                    session_id: view.id.clone(),
                    submission_id,
                };
                let session = view.id.clone();
                return self.act(
                    &session,
                    Act::Resume,
                    "_sill/submissions.resume",
                    json!(key),
                );
            }
            Message::DiscardHeld(submission_id) => {
                let Some(view) = self.current() else {
                    return Task::none();
                };
                let key = SubmissionKey {
                    session_id: view.id.clone(),
                    submission_id,
                };
                let session = view.id.clone();
                return self.act(
                    &session,
                    Act::Unqueue,
                    "_sill/submissions.cancel",
                    json!(key),
                );
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
            Message::Noop => {}
        }
        Task::none()
    }

    /// The pending changes and Settings → auto-revert, read off the GUI thread.
    fn changes_load(&self) -> Task<Message> {
        use changes_ui::Event;
        let Some(actions) = self.actions.clone() else {
            return Task::done(Message::Changes(Event::Loaded(Ok((demo::changes(), 0)))));
        };
        blocking(
            move || {
                let entries = actions
                    .reading(sill_client::changes::list)
                    .map_err(|e| format!("{e:#}"))?;
                // unread settings: no countdown rather than a wrong one
                let minutes = actions
                    .read::<sill_proto::admin::Settings>("_sill/settings.get", json!({}))
                    .map_or(0, |s| sill_client::changes::auto_revert(&s));
                Ok((entries, minutes))
            },
            |loaded| Message::Changes(Event::Loaded(loaded)),
        )
    }

    fn history_load(&self, before: Option<String>) -> Task<Message> {
        use changes_ui::Event;
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let append = before.is_some();
        blocking(
            move || {
                actions
                    .reading(|c| sill_client::changes::history(c, before.clone()))
                    .map_err(|e| format!("{e:#}"))
            },
            move |page| Message::Changes(Event::HistoryLoaded(page, append)),
        )
    }

    fn changes_event(&mut self, event: changes_ui::Event) -> Task<Message> {
        use changes_ui::Event;
        use sill_client::changes;
        use sill_proto::changes::Entry;
        let demo = self.demo;
        let Some(page) = &mut self.changes else {
            return Task::none();
        };
        match event {
            Event::Loaded(loaded) => {
                page.loading = false;
                match loaded {
                    Ok((entries, minutes)) => {
                        let keys: Vec<String> = entries.iter().map(changes::entry_key).collect();
                        page.open.retain(|k, _| keys.contains(k));
                        if page
                            .armed
                            .as_ref()
                            .is_some_and(|a| !keys.iter().any(|k| a.ends_with(k.as_str())))
                        {
                            page.armed = None;
                        }
                        page.auto_revert = minutes;
                        page.entries = Some(Ok(entries));
                    }
                    Err(error) => page.entries = Some(Err(error)),
                }
            }
            Event::Refresh => {
                if page.history {
                    page.history_loading = true;
                    return self.history_load(None);
                }
                page.loading = true;
                return self.changes_load();
            }
            Event::History(on) => {
                page.history = on;
                if on && page.events.is_empty() && !page.history_loading {
                    page.history_loading = true;
                    return self.history_load(None);
                }
            }
            Event::HistoryLoaded(loaded, append) => {
                page.history_loading = false;
                match loaded {
                    Ok(next) => {
                        if !append {
                            page.events.clear();
                        }
                        page.events.extend(next.events);
                        page.next = next.next;
                        page.history_error = None;
                    }
                    Err(error) => page.history_error = Some(error),
                }
            }
            Event::MoreHistory => {
                let before = page.next.clone();
                page.history_loading = true;
                return self.history_load(before);
            }
            Event::Diff(key) => {
                if page.open.remove(&key).is_some() {
                    return Task::none();
                }
                let Some(entry) = page.entry(&key) else {
                    return Task::none();
                };
                let target = changes::target(entry);
                page.open.insert(key.clone(), None);
                let Some(actions) = self.actions.clone() else {
                    return Task::done(Message::Changes(Event::DiffLoaded(
                        key,
                        Ok(demo::changes_diff()),
                    )));
                };
                return blocking(
                    move || {
                        actions
                            .reading(|c| changes::diff(c, target.clone()))
                            .map_err(|e| format!("{e:#}"))
                    },
                    move |diff| Message::Changes(Event::DiffLoaded(key, diff)),
                );
            }
            Event::DiffLoaded(key, diff) => {
                if let Some(slot) = page.open.get_mut(&key) {
                    *slot = Some(diff);
                }
            }
            event @ (Event::Keep(_)
            | Event::KeepAll(_)
            | Event::Revert(_)
            | Event::RevertAll(_)
            | Event::Undo(_)) => {
                let (keys, armed_as) = match &event {
                    Event::Keep(key) | Event::Undo(key) => (vec![key.clone()], None),
                    Event::KeepAll(keys) => (keys.clone(), None),
                    Event::Revert(key) => (vec![key.clone()], Some(key.clone())),
                    Event::RevertAll(keys) => {
                        (keys.clone(), keys.first().map(|k| format!("all {k}")))
                    }
                    _ => unreachable!(),
                };
                // reverting takes a second press
                if let Some(armed) = armed_as {
                    if page.armed.as_ref() != Some(&armed) {
                        page.armed = Some(armed);
                        return Task::none();
                    }
                    page.armed = None;
                }
                if demo {
                    page.said = Some("The demo keeps and reverts nothing.".into());
                    return Task::none();
                }
                let entries: Vec<&Entry> = keys.iter().filter_map(|k| page.entry(k)).collect();
                let Some(first) = entries.first() else {
                    return Task::none();
                };
                let (verb, (method, params)) = match (&event, first) {
                    (Event::Revert(_), Entry::Live { repo, path, .. }) => {
                        ("reverted", changes::git_revert(repo, path))
                    }
                    (
                        Event::Undo(_),
                        Entry::Kept {
                            session_id,
                            keep_id,
                            ..
                        },
                    ) => ("undone", changes::undo(session_id, keep_id)),
                    (Event::Keep(_) | Event::KeepAll(_), Entry::Overlay { session_id, .. }) => {
                        ("kept", changes::keep(session_id, &entries))
                    }
                    (Event::Revert(_) | Event::RevertAll(_), Entry::Overlay { session_id, .. }) => {
                        ("reverted", changes::revert(session_id, &entries))
                    }
                    _ => return Task::none(),
                };
                return self.act("", Act::Changes(verb), method, params);
            }
        }
        Task::none()
    }

    fn menu_event(&mut self, event: menu::Event) -> Task<Message> {
        use menu::Event;
        match event {
            Event::View(mode) => {
                self.menu = None;
                return self.set_mode(mode);
            }
            Event::Toggle => {
                if self.menu.take().is_some() {
                    return Task::none();
                }
                self.menu = Some(menu::Menu::default());
                let Some(actions) = self.actions.clone() else {
                    // the demo has two agents and nothing to reopen
                    let demo = ["claude", "codex"]
                        .iter()
                        .map(|id| {
                            serde_json::from_value(json!({
                                "id": id, "name": if *id == "claude" { "Claude Code" } else { "Codex" },
                                "command": []
                            }))
                            .expect("a demo agent")
                        })
                        .collect();
                    self.menu.as_mut().unwrap().agents = Some(Ok(demo));
                    return Task::none();
                };
                let settings = actions.clone();
                return Task::batch([
                    blocking(
                        move || {
                            actions
                                .reading(sill_client::sessions::startable)
                                .map_err(|e| format!("{e:#}"))
                        },
                        |agents| Message::Menu(Event::Agents(agents)),
                    ),
                    blocking(
                        move || {
                            settings
                                .read::<sill_proto::admin::Settings>(
                                    "_sill/settings.get",
                                    json!({}),
                                )
                                .ok()
                                .and_then(|s| s.remember_pins)
                                .unwrap_or(false)
                        },
                        |on| Message::Menu(Event::RememberPins(on)),
                    ),
                    self.list(),
                ]);
            }
            Event::Agents(agents) => {
                if let Some(menu) = &mut self.menu {
                    menu.agents = Some(agents);
                }
            }
            Event::RememberPins(on) => {
                if let Some(menu) = &mut self.menu {
                    menu.remember_pins = on;
                }
            }
            Event::Filter(value) => {
                if let Some(menu) = &mut self.menu {
                    menu.filter = value;
                }
            }
            Event::More => {
                if let Some(menu) = &mut self.menu {
                    menu.expanded = true;
                }
            }
            Event::New { agent, ghost } => {
                let remember = self.menu.take().is_some_and(|m| m.remember_pins);
                if self.demo {
                    return Task::none();
                }
                let (method, params) = sill_client::sessions::create(&agent, ghost, remember);
                return self.act("", Act::Create, method, params);
            }
            Event::Reopen(id) => {
                self.menu = None;
                let ended = self
                    .all_sessions
                    .iter()
                    .any(|s| s.session_id == id && s.ended_at.is_some());
                if !ended {
                    return self.choose(id);
                }
                let (method, params) = sill_client::sessions::reopen(&id);
                return self.act(&id, Act::Reopen(id.clone()), method, params);
            }
            Event::NewHere(id) => {
                self.menu = None;
                let Some(from) = self.all_sessions.iter().find(|s| s.session_id == id) else {
                    return Task::none();
                };
                let (method, params) = sill_client::sessions::new_here(from);
                return self.act("", Act::Create, method, params);
            }
            Event::Delete(id) => {
                let Some(menu) = &mut self.menu else {
                    return Task::none();
                };
                if menu.armed.as_deref() != Some(id.as_str()) {
                    menu.armed = Some(id);
                    return Task::none();
                }
                menu.armed = None;
                let (method, params) = sill_client::sessions::delete(&id);
                return self.act(&id, Act::Delete, method, params);
            }
        }
        Task::none()
    }

    /// Escape steps back, as in the shelf: the open file closes, then the transcript, then the
    /// drawer; with nothing left to close, the prompt takes the keyboard.
    fn escape(&mut self) -> Task<Message> {
        if self.menu.take().is_some() {
            return Task::none();
        }
        if self.changes.take().is_some() {
            return self.restore_scroll();
        }
        // full conversation is left first, as in the shelf: back to where it came from
        if self.layout.mode == layout::Mode::Full {
            return self.set_mode(self.layout.before_full);
        }
        self.armed_close = None;
        // the quick look closes; the drawer of Drawer mode stays
        let drawer = self.workspace.shown && self.layout.mode != layout::Mode::Drawer;
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        if view.files.reader.is_some() {
            view.files.close_reader();
            view.reader = None;
            return self.restore_scroll();
        }
        if view.text_view.is_some() {
            view.text_view = None;
            return self.restore_scroll();
        }
        let terminal = view.terminal_shown;
        if drawer {
            self.workspace.shown = false;
            return Task::none();
        }
        if terminal {
            return Task::none();
        }
        operation::focus("prompt")
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
        let snap = grew && selected && view.follow_tail && view.conversation_shown() && !selecting;
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
        if relist {
            tasks.push(self.list());
        }
        if snap {
            tasks.push(operation::snap_to_end("conversation"));
        }
        Task::batch(tasks)
    }

    fn listed(&mut self, result: Result<Vec<Session>, String>) -> Task<Message> {
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
        self.tabs = sessions
            .iter()
            .filter(|s| s.ended_at.is_none())
            .map(Tab::from)
            .collect();
        // a session that ended or went: its view goes, and with it its lane and terminal
        let open: BTreeSet<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        self.views.retain(|id, _| open.contains(id.as_str()));
        if self
            .selected
            .as_ref()
            .is_some_and(|s| !open.contains(s.as_str()))
        {
            self.selected = None;
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
        let mut tasks = vec![];
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
            tasks.push(self.choose(id));
        }
        if self.selected.is_none()
            && let Some(tab) = self.tabs.iter().find(|t| t.running)
        {
            let id = tab.id.clone();
            tasks.push(self.choose(id));
        }
        Task::batch(tasks)
    }

    fn choose(&mut self, id: String) -> Task<Message> {
        if !self.tabs.iter().any(|t| t.id == id) {
            return Task::none();
        }
        let again = self.selected.as_deref() == Some(id.as_str()) && self.changes.is_none();
        self.changes = None;
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
        let mut tasks = vec![self.restore_scroll()];
        if load_files {
            tasks.push(self.files_refresh(&id));
        }
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
            Some((id, Settled::Kept)) if view.draft.text().trim().is_empty() => {
                if let Some(pending) = view.outbox.take(&id) {
                    let reason = match &pending.state {
                        sill_client::outbox::State::Failed(reason) => reason.clone(),
                        _ => String::new(),
                    };
                    view.draft = text_editor::Content::with_text(pending.text());
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

    /// The prompt goes to the agent (the daemon queues it while a turn runs). It leaves the
    /// composer, but stays in the outbox until the daemon has it.
    fn send(&mut self) -> Task<Message> {
        let demo = self.demo;
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        let text = view.draft.text().trim_end().to_string();
        if text.trim().is_empty() {
            return Task::none();
        }
        view.draft = text_editor::Content::new();
        view.follow_tail = true;
        view.failed = None;
        if demo {
            for event in demo::prompt(view.conversation.seq + 1, &text) {
                view.accept(event);
            }
            return operation::snap_to_end("conversation");
        }
        let session = view.id.clone();
        let submit = view.outbox.send(&session, text);
        let reopen = view.lane.is_none();
        if reopen {
            self.open_lane(&session, lane::Mode::Open);
        }
        let id = submit.submission_id.clone();
        Task::batch([
            self.act(
                &session,
                Act::Send(id),
                "_sill/sessions.submit",
                json!(submit),
            ),
            operation::snap_to_end("conversation"),
        ])
    }

    /// Stop the turn. As the shelf does, prompts still queued are removed first: stopping first
    /// could let the turn's end dispatch the next one.
    fn stop(&mut self) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let (Some(view), Some(actions)) = (self.current(), self.actions.clone()) else {
            return Task::none();
        };
        let Some(turn_id) = view
            .conversation
            .session
            .as_ref()
            .and_then(|s| s.turn_id.clone())
        else {
            return Task::none();
        };
        let queued = view.conversation.queued_submission_ids();
        let session = view.id.clone();
        let session_id = session.clone();
        blocking(
            move || {
                let mut kept = vec![];
                for submission_id in queued {
                    let key = SubmissionKey {
                        session_id: session_id.clone(),
                        submission_id,
                    };
                    if let Some(problem) = actions
                        .call("_sill/submissions.cancel", json!(key))
                        .problem()
                    {
                        kept.push(problem.to_string());
                    }
                }
                let cancel = CancelTurn {
                    session_id,
                    turn_id,
                };
                let outcome = actions.call("_sill/sessions.cancel", json!(cancel));
                if kept.is_empty() || !outcome.is_done() {
                    outcome
                } else {
                    Outcome::Refused {
                        code: None,
                        message: format!(
                            "stop requested, but some queued prompts could not be removed: {}",
                            kept.join("; ")
                        ),
                    }
                }
            },
            move |outcome| Message::Done(session, Act::Stop, outcome),
        )
    }

    fn answer(&mut self, question_id: String, option_id: String) -> Task<Message> {
        let demo = self.demo;
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        view.follow_tail = view.follow_tail || view.conversation_shown();
        if demo {
            for event in demo::answered(view.conversation.seq + 1, &option_id) {
                view.accept(event);
            }
            return operation::snap_to_end("conversation");
        }
        let _ = view;
        self.answer_with(question_id, Answer::Permission { option_id })
    }

    /// One answer to a question of the selected session; a second press while it is on its way
    /// sends nothing.
    fn answer_with(&mut self, question_id: String, answer: Answer) -> Task<Message> {
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        if !view.answering.insert(question_id.clone()) {
            return Task::none();
        }
        view.failed = None;
        let params = AnswerQuestion {
            session_id: view.id.clone(),
            question_id: question_id.clone(),
            answer_id: sill_client::request_id("answer"),
            answer,
        };
        let session = view.id.clone();
        self.act(
            &session,
            Act::Answer(question_id),
            "_sill/questions.answer",
            json!(params),
        )
    }

    fn control(&mut self, control: Control) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        view.failed = None;
        let params = SessionControl {
            session_id: view.id.clone(),
            control,
        };
        let session = view.id.clone();
        self.act(
            &session,
            Act::Control,
            "_sill/sessions.control",
            json!(params),
        )
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
        if view.follow_tail && view.conversation_shown() && !self.selecting {
            operation::snap_to_end("conversation")
        } else {
            Task::none()
        }
    }

    fn toggle_terminal(&mut self) -> Task<Message> {
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        if view.terminal_shown {
            view.terminal_shown = false;
            return operation::focus("prompt");
        }
        view.terminal_shown = true;
        let reusable = view.terminal.as_ref().is_some_and(|t| {
            matches!(
                t.status,
                terminal::Status::Opening | terminal::Status::Running
            )
        });
        if let Some(pane) = view.terminal.as_mut().filter(|_| reusable) {
            pane.focused = true;
            return Task::none();
        }
        self.open_terminal()
    }

    /// A pane on the session's jail shell (the pretend one in `--demo`); its events come tagged
    /// so a replaced pane's late ones are dropped. The replaced pane detaches as it goes.
    fn open_terminal(&mut self) -> Task<Message> {
        let connect = self.connect.clone();
        let demo = self.demo;
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        view.terminal_generation += 1;
        let generation = view.terminal_generation;
        let (cols, rows) = view
            .terminal
            .as_ref()
            .map_or((80, 16), |t| (t.cols, t.rows));
        // the old pane goes first, so its stream is closed before another attaches
        view.terminal = None;
        let (pane, events) = match (demo, connect) {
            (false, Some(connect)) => terminal::Pane::open(
                cols,
                rows,
                sill_client::terminal::jail_shell(connect, view.id.clone()),
            ),
            _ => terminal::Pane::open(cols, rows, terminal::demo_shell),
        };
        view.terminal = Some(pane);
        let session = view.id.clone();
        let stream = iced::futures::stream::unfold(events, |events| async move {
            events.recv().await.ok().map(|event| (event, events))
        });
        Task::run(stream, move |event| {
            Message::Terminal(session.clone(), generation, event)
        })
    }
}

/// With `SILL_GUI_TRACE` set, what happened, on stderr: requests' outcomes, lanes' states.
fn trace(what: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SILL_GUI_TRACE").is_some()) {
        eprintln!("sill-gui: {}", what());
    }
}

fn toggle(set: &mut BTreeSet<String>, id: String) {
    if !set.remove(&id) {
        set.insert(id);
    }
}
