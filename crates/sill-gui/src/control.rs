//! One sill-gui per desktop account, and how the rest of the desktop reaches it: a Unix socket in
//! the runtime directory, `$XDG_RUNTIME_DIR/sill/gui.sock` (`sill toggle` writes to the same
//! path). The first sill-gui binds it; a second one started meanwhile sends the running one what
//! it was started to do (show it) and leaves. A command is one line: `show`, `hide`, `toggle`
//! with the X server time of the press that asked for it (0 when unknown), or `quit`.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

/// What the running window is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Show,
    Hide,
    /// Show, raise, or hide — as the hotkey; with the press's X server time.
    Toggle(u32),
    Quit,
}

impl Command {
    pub fn line(self) -> String {
        match self {
            Command::Show => "show".into(),
            Command::Hide => "hide".into(),
            Command::Toggle(time) => format!("toggle {time}"),
            Command::Quit => "quit".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        Some(match words.next()? {
            "show" => Command::Show,
            "hide" => Command::Hide,
            "toggle" => Command::Toggle(words.next().and_then(|t| t.parse().ok()).unwrap_or(0)),
            "quit" => Command::Quit,
            _ => return None,
        })
    }
}

/// The socket: `$SILL_GUI_SOCKET` when set (a second sill-gui beside the running one, for
/// trying a build), else `$XDG_RUNTIME_DIR/sill/gui.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SILL_GUI_SOCKET").map(PathBuf::from)
        && path.is_absolute()
    {
        return path;
    }
    sockets().join("gui.sock")
}

/// The socket of a window on the saved box `name`: `gui-NAME.sock` beside `gui.sock`, as
/// `sill box connect` gives one (sill's `frontend.rs`), so it opens beside this machine's window.
pub fn box_socket_path(name: &str) -> PathBuf {
    sockets().join(format!("gui-{name}.sock"))
}

fn sockets() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::temp_dir().join(format!("sill-{}", rustix::process::getuid().as_raw())));
    runtime.join("sill")
}

/// Send `command` to a running sill-gui; false when none answers.
pub fn send(command: Command) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(stream, "{}", command.line()).is_err() {
        return false;
    }
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).is_ok() && answer.trim() == "ok"
}

/// This process is the one sill-gui: the socket bound, its commands from now on sent to
/// `sink`. `None` when another sill-gui already answers on it.
pub fn claim(sink: impl Fn(Command) -> bool + Send + 'static) -> std::io::Result<Option<()>> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // someone answers: that is the one; nobody does: a socket left by one that died
            if UnixStream::connect(&path).is_ok() {
                return Ok(None);
            }
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path)?
        }
        Err(e) => return Err(e),
    };
    std::thread::Builder::new().name("sill-gui-control".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            let answer = match Command::parse(&line) {
                Some(command) if sink(command) => "ok",
                Some(_) => "gone",
                None => "unknown command",
            };
            let _ = writeln!(stream, "{answer}");
        }
    })?;
    Ok(Some(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_back() {
        for command in [Command::Show, Command::Hide, Command::Toggle(123), Command::Quit] {
            assert_eq!(Command::parse(&command.line()), Some(command));
        }
        assert_eq!(Command::parse("toggle"), Some(Command::Toggle(0)));
        assert_eq!(Command::parse("dance"), None);
    }
}
