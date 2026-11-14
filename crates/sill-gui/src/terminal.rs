//! The terminal pane: the shell the daemon keeps in the session's jail, drawn by this window.
//! alacritty_terminal is the emulator; the bytes come and go through sill-client's terminal
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
use sill_client::terminal::{self as link, Connection, Link, Replies};

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
            self.parser.advance(
                &mut *term,
                b"\x1b[2m[earlier output was not kept]\x1b[0m\r\n",
            );
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
    cache: canvas::Cache,
}

impl Pane {
    /// A pane whose terminal `connect` opens (or finds) off the GUI thread.
    pub fn open(
        cols: u16,
        rows: u16,
        connect: impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static,
    ) -> (Self, async_channel::Receiver<Event>) {
        let replies = Arc::new(OnceLock::new());
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            Listener(replies.clone()),
        )));
        let pending = Arc::new(AtomicBool::new(false));
        let (events, receiver) = async_channel::unbounded();
        let feed = Feed {
            term: term.clone(),
            parser: Processor::new(),
            pending: pending.clone(),
            events: events.clone(),
        };
        let link = Link::open(cols, rows, connect, feed, move |event| {
            let event = match event {
                link::Event::Opened => Event::Opened,
                link::Event::Failed(error) => Event::Failed(error),
                link::Event::Ended => Event::Ended,
            };
            events.send_blocking(event).is_ok()
        });
        let _ = replies.set(link.replies());
        let pane = Self {
            term,
            link,
            pending,
            cols,
            rows,
            status: Status::Opening,
            focused: true,
            cache: canvas::Cache::new(),
        };
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
                self.term.lock().selection =
                    Some(Selection::new(SelectionType::Simple, point, side));
            }
            Action::Drag(point, side) => {
                if let Some(selection) = &mut self.term.lock().selection {
                    selection.update(point, side);
                }
            }
            Action::Blur => self.focused = false,
            Action::Copy | Action::Paste => {} // the clipboard is the window's: see `selection`
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
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        (self.cols, self.rows) = (cols, rows);
        self.term
            .lock()
            .resize(TermSize::new(cols as usize, rows as usize));
        self.link.resize(cols, rows);
        self.cache.clear();
    }

    /// End the shell itself; the pane then shows that it ended.
    pub fn close(&self) {
        self.link.close();
    }

    pub fn selection(&self) -> Option<String> {
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    /// The visible screen as text, one line per row (tests and the snapshot check).
    #[cfg(test)]
    pub fn screen(&self) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|c| row[Column(c)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
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
    std::thread::Builder::new()
        .name("sill-gui-demo-shell".into())
        .spawn(move || {
            let mut line = String::new();
            let mut bytes = [0u8; 1024];
            let _ = write!(
                theirs,
                "\x1b[2mA pretend shell: attach to a session for a real one.\x1b[0m\r\n{PROMPT}"
            );
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
                                other => reply.push_str(&format!(
                                    "\x1b[31m{other}\x1b[0m: not in the demo\r\n"
                                )),
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
        stream: sill_proto::transport::RawStream {
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
}

impl canvas::Program<Action> for Pane {
    type State = Gesture;

    fn update(
        &self,
        state: &mut Gesture,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Action>> {
        use iced::Event as E;
        let publish = |a| Some(canvas::Action::publish(a).and_capture());
        match event {
            E::Window(iced::window::Event::RedrawRequested(_)) => {
                let (cols, rows) = grid_size(bounds.size());
                ((cols, rows) != (self.cols, self.rows))
                    .then(|| canvas::Action::publish(Action::Resize(cols, rows)))
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
                    let prefix: &[u8] = if mode.contains(TermMode::APP_CURSOR) {
                        b"\x1bO"
                    } else {
                        b"\x1b["
                    };
                    let bytes = (0..lines.abs())
                        .flat_map(|_| prefix.iter().copied().chain([key]))
                        .collect();
                    publish(Action::Input(bytes))
                } else {
                    publish(Action::Scroll(lines))
                }
            }
            E::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) if self.focused
                && self.status == Status::Running
                // the window's shortcuts (Ctrl+T hides the shell again) pass the shell by
                && !crate::keys::window_shortcut(key, *modifiers) =>
            {
                let shortcut = modifiers.control() && modifiers.shift();
                match key.as_ref() {
                    Key::Character(c) if shortcut && c.eq_ignore_ascii_case("c") => {
                        publish(Action::Copy)
                    }
                    Key::Character(c) if shortcut && c.eq_ignore_ascii_case("v") => {
                        publish(Action::Paste)
                    }
                    Key::Named(Named::PageUp) if modifiers.shift() => {
                        publish(Action::Scroll(self.rows as i32 - 1))
                    }
                    Key::Named(Named::PageDown) if modifiers.shift() => {
                        publish(Action::Scroll(1 - self.rows as i32))
                    }
                    _ => {
                        let mode = *self.term.lock().mode();
                        encode(key, *modifiers, text.as_deref(), mode)
                            .and_then(|b| publish(Action::Input(b)))
                    }
                }
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Gesture,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
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

    fn mouse_interaction(
        &self,
        _state: &Gesture,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
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
        let side = if x.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (Point::new(line, Column(column)), side)
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
        let width = if flags.contains(Flags::WIDE_CHAR) {
            2.0
        } else {
            1.0
        };
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
        let joins = run.as_ref().is_some_and(|(l, c, s, f, b)| {
            *l == line && c + s.chars().count() == column && *f == fg && *b == bold
        });
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
    let font = if bold {
        Font {
            weight: iced::font::Weight::Bold,
            ..MONO
        }
    } else {
        MONO
    };
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
    let width = if cell.flags.contains(Flags::WIDE_CHAR) {
        2.0
    } else {
        1.0
    };
    let size = Size::new(CELL_W * width, CELL_H);
    match content.cursor.shape {
        CursorShape::Hidden => {}
        _ if !focused => {
            let path = canvas::Path::rectangle(
                at + iced::Vector::new(0.5, 0.5),
                size - Size::new(1.0, 1.0),
            );
            frame.stroke(
                &path,
                Stroke::default().with_color(P.accent).with_width(1.0),
            );
        }
        CursorShape::Beam => frame.fill_rectangle(at, Size::new(2.0, CELL_H), P.accent),
        CursorShape::Underline => frame.fill_rectangle(
            at + iced::Vector::new(0.0, CELL_H - 2.0),
            Size::new(size.width, 2.0),
            P.accent,
        ),
        _ => {
            frame.fill_rectangle(at, size, P.accent);
            if cell.c != ' ' && cell.c != '\0' {
                put(
                    frame,
                    point.line.0,
                    point.column.0,
                    cell.c.to_string(),
                    P.bg,
                    false,
                );
            }
        }
    }
}

// --- colours and keys -----------------------------------------------------------------------

/// The 16 ANSI slots as the shelf maps them onto the theme: red is the error colour, green
/// success, yellow warning, blue and magenta the accent, cyan the link colour.
fn ansi16(index: usize) -> Color {
    let normal = [
        P.surface, P.error, P.success, P.warning, P.accent, P.accent, P.link, P.fg,
    ];
    let bright = [
        P.fg_dim, P.error, P.success, P.warning, P.accent, P.accent, P.link, P.fg,
    ];
    if index < 8 {
        normal[index]
    } else {
        bright[index - 8]
    }
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
pub fn encode(
    key: &Key,
    modifiers: Modifiers,
    text: Option<&str>,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let modifier = 1
        + u8::from(modifiers.shift())
        + 2 * u8::from(modifiers.alt())
        + 4 * u8::from(modifiers.control());
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
    let tilde = |n: u8| {
        if modifier > 1 {
            format!("\x1b[{n};{modifier}~")
        } else {
            format!("\x1b[{n}~")
        }
        .into_bytes()
    };
    let function = |c: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{c}")
        } else {
            format!("\x1bO{c}")
        }
        .into_bytes()
    };
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
    text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
        .map(esc)
}
