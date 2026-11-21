//! The terminal pane: the shell the daemon keeps in the session's jail, drawn by this window.
//! alacritty_terminal is the emulator; the bytes come and go through silli-client's terminal
//! [`Link`] over the daemon's byte stream, never a local pty. Hiding the pane keeps it attached;
//! dropping or replacing it detaches (the link closes its stream) and the shell keeps running.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as Ansi, CursorShape, NamedColor, Processor, Rgb};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::widget::canvas::{self, Frame, Geometry, Stroke};
use iced::{Color, Element, Fill, Font, Rectangle, Renderer, Size, Theme, mouse};
use silli_client::terminal::{self as link, Connection, Link, Replies};

use crate::style::{self, P};

/// Glyph size; Source Code Pro advances 600/1000 of an em, which fixes the cell's width.
const SIZE: f32 = style::MONO;
const CELL_W: f32 = SIZE * 0.6;
const CELL_H: f32 = (SIZE * 1.3) as u32 as f32;
const PAD: f32 = 6.0;
const MONO: Font = Font::with_name(crate::fonts::MONO_FAMILY);

/// What the pane hears from its link.
#[derive(Debug, Clone)]
pub enum Event {
    Opened,
    /// The screen changed; one is outstanding at a time however much arrives.
    Output,
    Failed(String),
    Ended,
}

/// What the person did in the pane.
#[derive(Debug, Clone)]
pub enum Action {
    Input(Vec<u8>),
    Resize(u16, u16),
    Scroll(i32),
    /// A press: the pane takes the keyboard and a selection starts here.
    Press(Point, Side),
    Drag(Point, Side),
    Blur,
    Copy,
    Paste,
    /// Paste the mouse's selection (the primary selection), as a middle click does.
    PastePrimary,
    /// Open a link in the browser: Ctrl+click on it.
    Open(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Opening,
    Running,
    Failed(String),
    Ended,
}

/// Answers the emulator writes back (device attributes, colour queries) go to the shell. The
/// handle does not keep the link alive: dropping the pane still detaches.
#[derive(Clone)]
struct Listener(Arc<OnceLock<Replies>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let reply = match event {
            TermEvent::PtyWrite(text) => text,
            TermEvent::ColorRequest(index, format) => format(rgb(index)),
            _ => return,
        };
        if let Some(replies) = self.0.get() {
            replies.send(reply.into_bytes());
        }
    }
}

/// The link's output into the emulator, on the link's reading thread.
struct Feed {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Processor<alacritty_terminal::vte::ansi::StdSyncHandler>,
    pending: Arc<AtomicBool>,
    events: async_channel::Sender<Event>,
}

impl link::Screen for Feed {
    fn begin(&mut self, cols: u16, rows: u16, discarded: bool) {
        // the replay is read at the size it was drawn at; the pane resizes it afterwards
        let mut term = self.term.lock();
        term.resize(TermSize::new(cols as usize, rows as usize));
        if discarded {
            self.parser.advance(&mut *term, b"\x1b[2m[earlier output was not kept]\x1b[0m\r\n");
        }
    }

    fn output(&mut self, bytes: &[u8]) -> bool {
        self.parser.advance(&mut *self.term.lock(), bytes);
        if !self.pending.swap(true, Ordering::AcqRel) {
            return self.events.send_blocking(Event::Output).is_ok();
        }
        true
    }
}

pub struct Pane {
    term: Arc<FairMutex<Term<Listener>>>,
    link: Link,
    pending: Arc<AtomicBool>,
    pub cols: u16,
    pub rows: u16,
    pub status: Status,
    pub focused: bool,
    /// The text pasted last, until the next key: a program may not show what it reads (a login's
    /// code prompt), so the pane says what went in.
    pub pasted: Option<String>,
    cache: canvas::Cache,
}

impl Pane {
    /// A pane whose terminal `connect` opens (or finds) off the GUI thread.
    pub fn open(cols: u16, rows: u16, connect: impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static) -> (Self, async_channel::Receiver<Event>) {
        let replies = Arc::new(OnceLock::new());
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener(replies.clone()))));
        let pending = Arc::new(AtomicBool::new(false));
        let (events, receiver) = async_channel::unbounded();
        let feed = Feed { term: term.clone(), parser: Processor::new(), pending: pending.clone(), events: events.clone() };
        let link = Link::open(cols, rows, connect, feed, move |event| {
            let event = match event {
                link::Event::Opened => Event::Opened,
                link::Event::Failed(error) => Event::Failed(error),
                link::Event::Ended => Event::Ended,
            };
            events.send_blocking(event).is_ok()
        });
        let _ = replies.set(link.replies());
        let pane = Self { term, link, pending, cols, rows, status: Status::Opening, focused: true, pasted: None, cache: canvas::Cache::new() };
        (pane, receiver)
    }

    pub fn accept(&mut self, event: Event) {
        match event {
            Event::Opened => {
                self.status = Status::Running;
                let term = self.term.lock();
                (self.cols, self.rows) = (term.columns() as u16, term.screen_lines() as u16);
            }
            Event::Output => self.pending.store(false, Ordering::Release),
            Event::Failed(error) => self.status = Status::Failed(error),
            Event::Ended => self.status = Status::Ended,
        }
        self.cache.clear();
    }

    pub fn perform(&mut self, action: Action) {
        match action {
            Action::Input(bytes) => self.input(bytes),
            Action::Resize(cols, rows) => self.resize(cols, rows),
            Action::Scroll(lines) => self.term.lock().scroll_display(Scroll::Delta(lines)),
            Action::Press(point, side) => {
                self.focused = true;
                self.term.lock().selection = Some(Selection::new(SelectionType::Simple, point, side));
            }
            Action::Drag(point, side) => {
                if let Some(selection) = &mut self.term.lock().selection {
                    selection.update(point, side);
                }
            }
            Action::Blur => self.focused = false,
            // the clipboard and the browser are the window's: see `selection`, `open_link`
            Action::Copy | Action::Paste | Action::PastePrimary | Action::Open(_) => {}
        }
        self.cache.clear();
    }

    /// Typed or pasted bytes go to the shell; the view follows them to the bottom.
    pub fn input(&mut self, bytes: Vec<u8>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.pasted = None;
        self.link.input(bytes);
        self.cache.clear();
    }

    /// Pasted text as the shell expects it: bracketed when it asked, newlines as Enter otherwise.
    pub fn paste(&mut self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let bytes = if bracketed {
            let clean = text.replace('\x1b', "");
            format!("\x1b[200~{clean}\x1b[201~").into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        };
        self.input(bytes);
        self.pasted = Some(text.to_string());
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        (self.cols, self.rows) = (cols, rows);
        self.term.lock().resize(TermSize::new(cols as usize, rows as usize));
        self.link.resize(cols, rows);
        self.cache.clear();
    }

    /// End the shell itself; the pane then shows that it ended.
    pub fn close(&self) {
        self.link.close();
    }

    pub fn selection(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    /// The visible screen as text, one line per row (tests and the snapshot check).
    #[cfg(test)]
    pub fn screen(&self) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns()).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The foreground colour of the cell at `line`, `column` (tests).
    #[cfg(test)]
    pub fn fg_at(&self, line: i32, column: usize) -> Color {
        let term = self.term.lock();
        let cell = &term.grid()[Line(line)][Column(column)];
        color(cell.fg, term.colors())
    }

    pub fn view(&self) -> Element<'_, Action> {
        iced::widget::canvas(self).width(Fill).height(Fill).into()
    }
}

/// `--demo`: a pretend shell on the other end of a socket pair; it echoes, knows `ls` and `exit`.
#[cfg(unix)]
pub fn demo_shell(cols: u16, rows: u16) -> anyhow::Result<Connection> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    const PROMPT: &str = "\x1b[32mdemo@jail\x1b[0m:\x1b[34m~/project\x1b[0m$ ";
    let (ours, mut theirs) = UnixStream::pair()?;
    std::thread::Builder::new().name("silli-gui-demo-shell".into()).spawn(move || {
        let mut line = String::new();
        let mut bytes = [0u8; 1024];
        let _ = write!(theirs, "\x1b[2mA pretend shell: attach to a session for a real one.\x1b[0m\r\n{PROMPT}");
        while let Ok(n) = theirs.read(&mut bytes) {
            if n == 0 {
                break;
            }
            let mut reply = String::new();
            for &b in &bytes[..n] {
                match b {
                    b'\r' => {
                        reply.push_str("\r\n");
                        match line.trim() {
                            "" => {}
                            "exit" => {
                                let _ = theirs.write_all(reply.as_bytes());
                                return;
                            }
                            "ls" => reply.push_str(
                                "\x1b[1;34msrc\x1b[0m  \x1b[1;34mtests\x1b[0m  Cargo.toml  \
                                     README.md  \x1b[1;32mbuild.sh\x1b[0m\r\n",
                            ),
                            other => reply.push_str(&format!("\x1b[31m{other}\x1b[0m: not in the demo\r\n")),
                        }
                        line.clear();
                        reply.push_str(PROMPT);
                    }
                    0x7f | 0x08 => {
                        if line.pop().is_some() {
                            reply.push_str("\x08 \x08");
                        }
                    }
                    0x03 => {
                        line.clear();
                        reply.push_str("^C\r\n");
                        reply.push_str(PROMPT);
                    }
                    b if b >= 0x20 && b != 0x7f => {
                        line.push(b as char);
                        reply.push(b as char);
                    }
                    _ => {}
                }
            }
            if theirs.write_all(reply.as_bytes()).is_err() {
                break;
            }
        }
    })?;
    let close = ours.try_clone()?;
    Ok(Connection {
        discarded: false,
        size: (cols, rows),
        stream: silli_proto::transport::RawStream {
            reader: Box::new(ours.try_clone()?),
            writer: Box::new(ours),
            interrupt: Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
            shutdown_write: Arc::new(|| {}),
        },
        control: Box::new(|_| Ok(())),
    })
}

/// Off Unix the demo has no pretend shell.
#[cfg(not(unix))]
pub fn demo_shell(_: u16, _: u16) -> anyhow::Result<Connection> {
    anyhow::bail!("the demo's pretend shell needs a Unix socket pair")
}

// --- drawing and input ----------------------------------------------------------------------

/// Columns and rows that fit `size`.
pub fn grid_size(size: Size) -> (u16, u16) {
    let cols = ((size.width - 2.0 * PAD) / CELL_W).floor().max(20.0) as u16;
    let rows = ((size.height - 2.0 * PAD) / CELL_H).floor().max(5.0) as u16;
    (cols, rows)
}

#[derive(Default)]
pub struct Gesture {
    selecting: bool,
    /// The keyboard's modifiers: a click is a link's with Ctrl held.
    modifiers: Modifiers,
}

impl canvas::Program<Action> for Pane {
    type State = Gesture;

    fn update(&self, state: &mut Gesture, event: &canvas::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Action>> {
        use iced::Event as E;
        let publish = |a| Some(canvas::Action::publish(a).and_capture());
        match event {
            E::Window(iced::window::Event::RedrawRequested(_)) => {
                let (cols, rows) = grid_size(bounds.size());
                ((cols, rows) != (self.cols, self.rows)).then(|| canvas::Action::publish(Action::Resize(cols, rows)))
            }
            E::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
                None
            }
            // Ctrl+click (Cmd+click on macOS) on a link opens it (a login's link, longer than
            // the pane is wide)
            E::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if state.modifiers.command()
                    && let Some(at) = cursor.position_in(bounds)
                    && let Some(url) = self.link_at(self.cell_at(at).0) =>
            {
                publish(Action::Open(url))
            }
            E::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)) if cursor.is_over(bounds) && self.status == Status::Running => {
                publish(Action::PastePrimary)
            }
            E::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                match cursor.position_in(bounds) {
                    Some(at) => {
                        state.selecting = true;
                        let (point, side) = self.cell_at(at);
                        publish(Action::Press(point, side))
                    }
                    // a press elsewhere takes the keyboard away, without taking the press
                    None if self.focused => Some(canvas::Action::publish(Action::Blur)),
                    None => None,
                }
            }
            E::Mouse(mouse::Event::CursorMoved { .. }) if state.selecting => {
                let at = cursor.position_from(bounds.position())?;
                let (point, side) = self.cell_at(at);
                publish(Action::Drag(point, side))
            }
            E::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.selecting => {
                state.selecting = false;
                None
            }
            E::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let lines = match *delta {
                    mouse::ScrollDelta::Lines { y, .. } => (y * 3.0).round() as i32,
                    mouse::ScrollDelta::Pixels { y, .. } => (y / CELL_H).round() as i32,
                };
                if lines == 0 {
                    return Some(canvas::Action::capture());
                }
                let mode = *self.term.lock().mode();
                if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    // a full-screen program (less, vim) scrolls by its arrow keys
                    let key = if lines > 0 { b'A' } else { b'B' };
                    let prefix: &[u8] = if mode.contains(TermMode::APP_CURSOR) { b"\x1bO" } else { b"\x1b[" };
                    let bytes = (0..lines.abs()).flat_map(|_| prefix.iter().copied().chain([key])).collect();
                    publish(Action::Input(bytes))
                } else {
                    publish(Action::Scroll(lines))
                }
            }
            E::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. }) if self.focused && self.status == Status::Running => {
                match key.as_ref() {
                    _ if let Some(action) = clipboard_key(key, *modifiers) => publish(action),
                    // other Cmd chords are the window's shortcuts, never the shell's input
                    _ if cfg!(target_os = "macos") && modifiers.logo() => None,
                    Key::Named(Named::Insert) if modifiers.shift() && !modifiers.control() => publish(Action::Paste),
                    Key::Named(Named::PageUp) if modifiers.shift() => publish(Action::Scroll(self.rows as i32 - 1)),
                    Key::Named(Named::PageDown) if modifiers.shift() => publish(Action::Scroll(1 - self.rows as i32)),
                    _ => {
                        let mode = *self.term.lock().mode();
                        encode(key, *modifiers, text.as_deref(), mode).and_then(|b| publish(Action::Input(b)))
                    }
                }
            }
            _ => None,
        }
    }

    fn draw(&self, _state: &Gesture, renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let term = self.term.lock();
        let screen = self.cache.draw(renderer, bounds.size(), |frame| {
            frame.fill_rectangle(iced::Point::ORIGIN, frame.size(), P.bg);
            draw_cells(frame, &term);
        });
        let mut cursor = Frame::new(renderer, bounds.size());
        if self.status == Status::Running {
            draw_cursor(&mut cursor, &term, self.focused);
        }
        vec![screen, cursor.into_geometry()]
    }

    fn mouse_interaction(&self, state: &Gesture, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if state.modifiers.command()
            && let Some(at) = cursor.position_in(bounds)
            && self.link_at(self.cell_at(at).0).is_some()
        {
            return mouse::Interaction::Pointer;
        }
        if cursor.is_over(bounds) { mouse::Interaction::Text } else { mouse::Interaction::default() }
    }
}

impl Pane {
    /// The grid cell under a point of the pane, and which half of it.
    fn cell_at(&self, at: iced::Point) -> (Point, Side) {
        let term = self.term.lock();
        let x = ((at.x - PAD) / CELL_W).max(0.0);
        let y = ((at.y - PAD) / CELL_H).max(0.0);
        let column = (x as usize).min(term.columns().saturating_sub(1));
        let row = (y as usize).min(term.screen_lines().saturating_sub(1));
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (Point::new(line, Column(column)), side)
    }

    /// The web link at `point`: the cell's own (OSC 8), else an `http(s)://` address written
    /// there, read across wrapped lines (a login's link is longer than the pane is wide).
    pub fn link_at(&self, point: Point) -> Option<String> {
        let term = self.term.lock();
        let grid = term.grid();
        if point.line < grid.topmost_line() || point.line > grid.bottommost_line() {
            return None;
        }
        if let Some(link) = grid[point].hyperlink() {
            return web(link.uri());
        }
        let columns = term.columns();
        let wraps = |line: Line| grid[line][Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        let mut first = point.line;
        while first > grid.topmost_line() && wraps(Line(first.0 - 1)) {
            first = Line(first.0 - 1);
        }
        let mut last = point.line;
        while last < grid.bottommost_line() && wraps(last) {
            last = Line(last.0 + 1);
        }
        let mut chars = vec![];
        let mut at = 0;
        for line in first.0..=last.0 {
            for column in 0..columns {
                let cell = &grid[Line(line)][Column(column)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if Line(line) == point.line && column == point.column.0 {
                    at = chars.len();
                }
                chars.push(cell.c);
            }
        }
        link_in(&chars, at)
    }
}

/// The `http(s)://` address in `chars` that `at` is on, if any.
fn link_in(chars: &[char], at: usize) -> Option<String> {
    let part = |c: char| !c.is_whitespace() && c != '\0' && !"<>\"'`".contains(c);
    if !chars.get(at).is_some_and(|c| part(*c)) {
        return None;
    }
    let mut start = at;
    while start > 0 && part(chars[start - 1]) {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < chars.len() && part(chars[end + 1]) {
        end += 1;
    }
    let word: String = chars[start..=end].iter().collect();
    let begin = word.find("https://").or_else(|| word.find("http://"))?;
    // the press is on the address, not on what runs into it from the left
    let before = word[..begin].chars().count();
    if at - start < before {
        return None;
    }
    let url = word[begin..].trim_end_matches(|c| ".,;:!?)]}".contains(c));
    web(url)
}

/// `url` when it is a web address: only those open from a terminal's output.
fn web(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then(|| url.to_string())
}

/// The terminal's copy and paste keys, in the platform's way. Linux and Windows: Ctrl+Shift+C/V
/// as terminals have them, and Ctrl+V too, as the rest of the window pastes (a login's code).
/// macOS: Cmd+C/V as Terminal has them; every Ctrl key goes to the shell there.
fn clipboard_key(key: &Key, modifiers: Modifiers) -> Option<Action> {
    let Key::Character(c) = key.as_ref() else { return None };
    let (copy, paste) = if cfg!(target_os = "macos") {
        let cmd = modifiers.logo() && !modifiers.alt() && !modifiers.control();
        (cmd, cmd)
    } else {
        (modifiers.control() && modifiers.shift(), modifiers.control() && !modifiers.alt())
    };
    if copy && c.eq_ignore_ascii_case("c") {
        Some(Action::Copy)
    } else if paste && c.eq_ignore_ascii_case("v") {
        Some(Action::Paste)
    } else {
        None
    }
}

/// Cmd+C/V on macOS and Ctrl(+Shift)+C/V elsewhere; on macOS Ctrl+C and Ctrl+V stay the
/// shell's (an interrupt, a literal next character).
#[test]
fn copy_and_paste_keys_follow_the_platform() {
    let ch = |c: &str| Key::Character(c.into());
    let is = |key: &str, modifiers: Modifiers| clipboard_key(&ch(key), modifiers);
    let paste = |a: Option<Action>| matches!(a, Some(Action::Paste));
    let copy = |a: Option<Action>| matches!(a, Some(Action::Copy));
    if cfg!(target_os = "macos") {
        assert!(paste(is("v", Modifiers::LOGO)));
        assert!(copy(is("c", Modifiers::LOGO)));
        assert!(is("v", Modifiers::CTRL).is_none());
        assert!(is("c", Modifiers::CTRL).is_none());
    } else {
        assert!(paste(is("v", Modifiers::CTRL)));
        assert!(paste(is("v", Modifiers::CTRL | Modifiers::SHIFT)));
        assert!(copy(is("c", Modifiers::CTRL | Modifiers::SHIFT)));
        assert!(is("c", Modifiers::CTRL).is_none(), "Ctrl+C is the shell's interrupt");
        assert!(is("v", Modifiers::LOGO).is_none());
    }
    assert!(is("x", Modifiers::COMMAND).is_none());
}

/// Open `url` in the person's browser, off the GUI thread; a failure is said on stderr.
pub fn open_link(url: String) {
    // Whoever names the address (terminal output, a message, the daemon of a box or of the VM):
    // only a web address goes to the system's opener, never a file, another scheme or an option
    // (audit 2026-11-15 A8).
    let Some(url) = web(&url).filter(|u| !u.chars().any(|c| c.is_control() || c.is_whitespace())) else {
        eprintln!("silli: not opening {url:?}: not a web address");
        return;
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(&url).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    match command.spawn() {
        // reaped off the GUI thread, so it leaves no zombie
        Ok(mut child) => {
            let _ = std::thread::Builder::new().name("silli-gui-open".into()).spawn(move || child.wait());
        }
        Err(e) => eprintln!("silli-gui: opening {url}: {e}"),
    }
}

fn origin(line: i32, column: usize) -> iced::Point {
    iced::Point::new(PAD + column as f32 * CELL_W, PAD + line as f32 * CELL_H)
}

/// Text in runs of cells that share a style; a wide or composed character is placed on its own,
/// so the grid stays aligned however the font shapes it.
fn draw_cells(frame: &mut Frame, term: &Term<Listener>) {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let colors = content.colors;
    let selection = content.selection;
    let mut run: Option<(i32, usize, String, Color, bool)> = None;
    let flush = |frame: &mut Frame, run: &mut Option<(i32, usize, String, Color, bool)>| {
        if let Some((line, column, content, color, bold)) = run.take() {
            put(frame, line, column, content, color, bold);
        }
    };
    for cell in content.display_iter {
        let flags = cell.flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let line = cell.point.line.0 + offset;
        let column = cell.point.column.0;
        let (mut fg, mut bg) = (color(cell.fg, colors), color(cell.bg, colors));
        let bold = flags.intersects(Flags::BOLD);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.intersects(Flags::DIM) {
            fg = style::mix(fg, bg, 0.4);
        }
        if selection.is_some_and(|s| s.contains(cell.point)) {
            bg = style::alpha(P.accent, 0.35);
        }
        let width = if flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
        if bg != P.bg {
            frame.fill_rectangle(origin(line, column), Size::new(CELL_W * width, CELL_H), bg);
        }
        if flags.intersects(Flags::ALL_UNDERLINES) {
            let at = origin(line, column) + iced::Vector::new(0.0, CELL_H - 2.0);
            frame.fill_rectangle(at, Size::new(CELL_W * width, 1.0), fg);
        }
        if flags.contains(Flags::STRIKEOUT) {
            let at = origin(line, column) + iced::Vector::new(0.0, CELL_H / 2.0);
            frame.fill_rectangle(at, Size::new(CELL_W * width, 1.0), fg);
        }
        let hidden = flags.contains(Flags::HIDDEN) || cell.c == ' ' || cell.c == '\0';
        let plain = cell.c.is_ascii() && width == 1.0 && cell.zerowidth().is_none();
        let joins = run.as_ref().is_some_and(|(l, c, s, f, b)| *l == line && c + s.chars().count() == column && *f == fg && *b == bold);
        if hidden || !plain || !joins {
            flush(frame, &mut run);
        }
        if hidden {
            continue;
        }
        if plain {
            match &mut run {
                Some((_, _, s, _, _)) => s.push(cell.c),
                None => run = Some((line, column, cell.c.to_string(), fg, bold)),
            }
        } else {
            let mut composed = cell.c.to_string();
            composed.extend(cell.zerowidth().into_iter().flatten());
            put(frame, line, column, composed, fg, bold);
        }
    }
    flush(frame, &mut run);
}

fn put(frame: &mut Frame, line: i32, column: usize, content: String, color: Color, bold: bool) {
    let font = if bold { Font { weight: iced::font::Weight::Bold, ..MONO } } else { MONO };
    frame.fill_text(canvas::Text {
        content,
        position: origin(line, column),
        color,
        size: SIZE.into(),
        line_height: iced::widget::text::LineHeight::Absolute(CELL_H.into()),
        font,
        shaping: iced::widget::text::Shaping::Advanced,
        ..canvas::Text::default()
    });
}

/// A block in the accent colour while the pane has the keyboard, an outline when it has not.
fn draw_cursor(frame: &mut Frame, term: &Term<Listener>, focused: bool) {
    let content = term.renderable_content();
    if !content.mode.contains(TermMode::SHOW_CURSOR) || content.display_offset != 0 {
        return;
    }
    let point = content.cursor.point;
    let at = origin(point.line.0, point.column.0);
    let cell = &term.grid()[point];
    let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
    let size = Size::new(CELL_W * width, CELL_H);
    match content.cursor.shape {
        CursorShape::Hidden => {}
        _ if !focused => {
            let path = canvas::Path::rectangle(at + iced::Vector::new(0.5, 0.5), size - Size::new(1.0, 1.0));
            frame.stroke(&path, Stroke::default().with_color(P.accent).with_width(1.0));
        }
        CursorShape::Beam => frame.fill_rectangle(at, Size::new(2.0, CELL_H), P.accent),
        CursorShape::Underline => frame.fill_rectangle(at + iced::Vector::new(0.0, CELL_H - 2.0), Size::new(size.width, 2.0), P.accent),
        _ => {
            frame.fill_rectangle(at, size, P.accent);
            if cell.c != ' ' && cell.c != '\0' {
                put(frame, point.line.0, point.column.0, cell.c.to_string(), P.bg, false);
            }
        }
    }
}

// --- colours and keys -----------------------------------------------------------------------

/// The 16 ANSI slots mapped onto the theme: red is the error colour, green
/// success, yellow warning, blue and magenta the accent, cyan the link colour.
fn ansi16(index: usize) -> Color {
    let normal = [P.surface, P.error, P.success, P.warning, P.accent, P.accent, P.link, P.fg];
    let bright = [P.fg_dim, P.error, P.success, P.warning, P.accent, P.accent, P.link, P.fg];
    if index < 8 { normal[index] } else { bright[index - 8] }
}

fn from_rgb(c: Rgb) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

fn to_rgb(c: Color) -> Rgb {
    let [r, g, b, _] = c.into_rgba8();
    Rgb { r, g, b }
}

/// The default for one of the 256 indexed colours, or fg (256) / bg (257) / cursor (258).
fn palette(index: usize) -> Color {
    match index {
        0..16 => ansi16(index),
        16..232 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Color::from_rgb8(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..256 => {
            let v = (8 + (index - 232) * 10) as u8;
            Color::from_rgb8(v, v, v)
        }
        256 => P.fg,
        257 => P.bg,
        258 => P.accent,
        _ => P.fg,
    }
}

fn rgb(index: usize) -> Rgb {
    to_rgb(palette(index))
}

fn color(c: Ansi, colors: &Colors) -> Color {
    let indexed = |i: usize| colors[i].map(from_rgb).unwrap_or_else(|| palette(i));
    match c {
        Ansi::Spec(rgb) => from_rgb(rgb),
        Ansi::Indexed(i) => indexed(i as usize),
        Ansi::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground => indexed(256),
            NamedColor::Background => indexed(257),
            NamedColor::Cursor => indexed(258),
            NamedColor::DimForeground => P.fg_dim,
            dim if (dim as usize) >= NamedColor::DimBlack as usize => {
                let base = dim as usize - NamedColor::DimBlack as usize;
                style::mix(indexed(base), P.bg, 0.4)
            }
            other => indexed(other as usize),
        },
    }
}

/// The bytes a key sends, as xterm does: cursor keys follow the application-cursor mode and carry
/// their modifiers (`ESC [1;5A` for Ctrl+Up), Ctrl+letter is its control code, Alt prefixes ESC.
pub fn encode(key: &Key, modifiers: Modifiers, text: Option<&str>, mode: TermMode) -> Option<Vec<u8>> {
    let modifier = 1 + u8::from(modifiers.shift()) + 2 * u8::from(modifiers.alt()) + 4 * u8::from(modifiers.control());
    let esc = |s: &str| {
        let mut b = Vec::new();
        if modifiers.alt() {
            b.push(0x1b);
        }
        b.extend_from_slice(s.as_bytes());
        b
    };
    let cursor = |c: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{c}")
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
        .into_bytes()
    };
    let tilde = |n: u8| if modifier > 1 { format!("\x1b[{n};{modifier}~") } else { format!("\x1b[{n}~") }.into_bytes();
    let function = |c: char| if modifier > 1 { format!("\x1b[1;{modifier}{c}") } else { format!("\x1bO{c}") }.into_bytes();
    if let Key::Named(named) = key {
        let bytes = match named {
            Named::Enter => esc("\r"),
            Named::Backspace if modifiers.control() => esc("\x08"),
            Named::Backspace => esc("\x7f"),
            Named::Tab if modifiers.shift() => b"\x1b[Z".to_vec(),
            Named::Tab => esc("\t"),
            Named::Escape => b"\x1b".to_vec(),
            Named::Space if modifiers.control() => vec![0],
            Named::ArrowUp => cursor('A'),
            Named::ArrowDown => cursor('B'),
            Named::ArrowRight => cursor('C'),
            Named::ArrowLeft => cursor('D'),
            Named::Home => cursor('H'),
            Named::End => cursor('F'),
            Named::Insert => tilde(2),
            Named::Delete => tilde(3),
            Named::PageUp => tilde(5),
            Named::PageDown => tilde(6),
            Named::F1 => function('P'),
            Named::F2 => function('Q'),
            Named::F3 => function('R'),
            Named::F4 => function('S'),
            Named::F5 => tilde(15),
            Named::F6 => tilde(17),
            Named::F7 => tilde(18),
            Named::F8 => tilde(19),
            Named::F9 => tilde(20),
            Named::F10 => tilde(21),
            Named::F11 => tilde(23),
            Named::F12 => tilde(24),
            _ => return text.filter(|t| !t.is_empty()).map(esc),
        };
        return Some(bytes);
    }
    if modifiers.control()
        && let Key::Character(c) = key
        && let Some(ch) = c.chars().next()
    {
        let code = match ch.to_ascii_lowercase() {
            ch @ 'a'..='z' => ch as u8 - b'a' + 1,
            '@' | ' ' | '2' => 0,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '/' | '7' => 0x1f,
            '8' | '?' => 0x7f,
            _ => return None,
        };
        let mut bytes = if modifiers.alt() { vec![0x1b] } else { vec![] };
        bytes.push(code);
        return Some(bytes);
    }
    text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control)).map(esc)
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn a_link_is_found_where_it_is_pressed() {
        let line = chars("Browser didn't open? Use https://claude.ai/oauth?code=1&state=x. Then");
        let on = line.iter().position(|c| *c == 'o').unwrap() + 30;
        assert_eq!(link_in(&line, on).as_deref(), Some("https://claude.ai/oauth?code=1&state=x"), "the sentence's full stop is not the link's");
        assert_eq!(link_in(&line, 3), None, "a press on the words before it");
        let run_in = chars("(see:https://a.example/x)");
        assert_eq!(link_in(&run_in, 2), None, "text running into it is not the link");
        assert_eq!(link_in(&run_in, 10).as_deref(), Some("https://a.example/x"));
        assert_eq!(link_in(&chars("file:///etc/passwd"), 3), None, "web links only");
        assert_eq!(web("javascript:alert(1)"), None);
    }

    #[test]
    fn terminal_events_from_a_closed_view_cannot_reach_its_reopened_pane() {
        let (mut app, _) = crate::App::new(true, None);
        let old_events = app.open_terminal("demo");
        let old = app.views["demo"].terminal_generation;
        app.views.insert("demo".into(), crate::SessionView::new("demo"));
        let new_events = app.open_terminal("demo");
        let current = app.views["demo"].terminal_generation;
        assert_ne!(old, current);
        let _ = app.update(crate::Message::Terminal("demo".into(), old, Event::Ended));
        assert_eq!(app.views["demo"].terminal.as_ref().unwrap().status, Status::Opening);
        let _ = app.update(crate::Message::Terminal("demo".into(), current, Event::Opened));
        assert_eq!(app.views["demo"].terminal.as_ref().unwrap().status, Status::Running);
        drop((old_events, new_events));
    }

    #[test]
    fn clipboard_completion_is_discarded_after_switching_shells_or_tabs() {
        let (mut app, _) = crate::App::new(true, None);
        let first_events = app.open_terminal("demo");
        let first = app.views["demo"].terminal_generation;
        // Replacing/switching the shell does not redirect the pending paste.
        let second_events = app.open_terminal("demo");
        let second = app.views["demo"].terminal_generation;
        let _ = app.update(crate::Message::TermPaste("demo".into(), first, Some("old shell".into())));
        assert_eq!(app.views["demo"].terminal.as_ref().unwrap().pasted, None);
        // Switching tabs also cannot deliver text into the newly selected terminal.
        app.views.insert("other".into(), crate::SessionView::new("other"));
        app.selected = Some("other".into());
        let other_events = app.open_terminal("other");
        let _ = app.update(crate::Message::TermPaste("demo".into(), second, Some("old tab".into())));
        assert_eq!(app.views["other"].terminal.as_ref().unwrap().pasted, None);
        app.selected = Some("demo".into());
        let _ = app.update(crate::Message::TermPaste("demo".into(), second, Some("current".into())));
        assert_eq!(app.views["demo"].terminal.as_ref().unwrap().pasted.as_deref(), Some("current"));
        drop((first_events, second_events, other_events));
    }

    /// A paste is said in the pane's strip until the next key, whatever the program shows.
    #[test]
    fn a_paste_is_shown_until_the_next_key() {
        let (mut pane, _events) = Pane::open(40, 12, demo_shell);
        crate::tests::settle(&pane, "project$");
        pane.paste("code#123");
        assert_eq!(pane.pasted.as_deref(), Some("code#123"));
        crate::tests::settle(&pane, "code#123");
        pane.input(b"x".to_vec());
        assert_eq!(pane.pasted, None);
    }

    /// A login link longer than the pane is wide is one link across its wrapped lines.
    #[test]
    fn a_wrapped_link_is_read_whole() {
        let url = format!("https://claude.ai/oauth/authorize?code=true&state={}", "a".repeat(60));
        let (mut pane, _events) = Pane::open(40, 12, demo_shell);
        crate::tests::settle(&pane, "project$");
        pane.input(url.clone().into_bytes());
        crate::tests::settle(&pane, "aaaa");
        // the prompt line, then the link's rest on the lines under it
        let line = (0..12).find(|l| pane.screen().lines().nth(*l).is_some_and(|t| t.ends_with("aaaa"))).unwrap();
        let found = pane.link_at(Point::new(Line(line as i32), Column(5)));
        assert_eq!(found.as_deref(), Some(url.as_str()));
    }
}
