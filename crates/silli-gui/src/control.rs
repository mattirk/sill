//! One silli-gui per desktop account, and how the rest of the desktop reaches it: a Unix socket in
//! the runtime directory, `$XDG_RUNTIME_DIR/silli/gui.sock` (`silli toggle` writes to the same
//! path); on Windows a named pipe of the account, `\.pipesilli-gui-ACCOUNT`, with the same lines. The first silli-gui binds it; a second one started meanwhile sends the running one what
//! it was started to do (show it) and leaves. A command is one line: `show`, `hide`, `toggle`
//! with the X server time of the press that asked for it (0 when unknown), or `quit`.
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
#[cfg(unix)]
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

/// The socket: `$SILLI_GUI_SOCKET` when set (a second silli-gui beside the running one, for
/// trying a build), else `$XDG_RUNTIME_DIR/silli/gui.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SILLI_GUI_SOCKET").map(PathBuf::from)
        && path.is_absolute()
    {
        return path;
    }
    #[cfg(unix)]
    return sockets().join("gui.sock");
    #[cfg(windows)]
    pipe_name(None)
}

/// The socket of a window on the saved box `name`: `gui-NAME.sock` beside `gui.sock`, as
/// `silli box connect` gives one (silli's `frontend.rs`), so it opens beside this machine's window.
pub fn box_socket_path(name: &str) -> PathBuf {
    #[cfg(unix)]
    return sockets().join(format!("gui-{name}.sock"));
    #[cfg(windows)]
    pipe_name(Some(name))
}

/// The pipe of this account's window, or of its window on the box `name`.
#[cfg(windows)]
fn pipe_name(name: Option<&str>) -> PathBuf {
    let account: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    let name = name.map(|name| format!("-{name}")).unwrap_or_default();
    PathBuf::from(format!(r"\.pipesilli-gui{name}-{account}"))
}

#[cfg(unix)]
fn sockets() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::temp_dir().join(format!("silli-{}", rustix::process::getuid().as_raw())));
    runtime.join("silli")
}

/// Send `command` to a running silli-gui; false when none answers.
#[cfg(unix)]
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

/// This process is the one silli-gui: the socket bound, its commands from now on sent to
/// `sink`. `None` when another silli-gui already answers on it.
#[cfg(unix)]
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
    std::thread::Builder::new().name("silli-gui-control".into()).spawn(move || {
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

/// Windows' named pipe, where the Unix socket is elsewhere: one instance, served one caller at a
/// time. Whoever makes the first instance is the one silli-gui; a pipe's own access keeps other
/// accounts from writing to it.
#[cfg(windows)]
mod pipe {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, instances: u32, out: u32, input: u32, timeout: u32, security: *const c_void)
        -> *mut c_void;
        fn ConnectNamedPipe(pipe: *mut c_void, overlapped: *mut c_void) -> i32;
        fn DisconnectNamedPipe(pipe: *mut c_void) -> i32;
        fn FlushFileBuffers(file: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    const DUPLEX: u32 = 0x3;
    const FIRST_INSTANCE: u32 = 0x0008_0000;
    /// Bytes, waiting calls, no callers from other machines.
    const BYTES_LOCAL_ONLY: u32 = 0x8;
    const INVALID: *mut c_void = usize::MAX as *mut c_void;
    const ACCESS_DENIED: u32 = 5;
    const PIPE_BUSY: u32 = 231;
    const PIPE_CONNECTED: u32 = 535;

    /// The server's end of the pipe at `path`, or `None` when a pipe of that name exists already.
    pub struct Server(*mut c_void);
    // SAFETY: a kernel handle, used by the one thread that owns the server
    unsafe impl Send for Server {}

    impl Server {
        pub fn claim(path: &std::path::Path) -> std::io::Result<Option<Server>> {
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            // SAFETY: a NUL-terminated name, default security
            let pipe = unsafe { CreateNamedPipeW(name.as_ptr(), DUPLEX | FIRST_INSTANCE, BYTES_LOCAL_ONLY, 1, 4096, 4096, 0, std::ptr::null()) };
            if pipe == INVALID {
                // SAFETY: reads this thread's last error
                return match unsafe { GetLastError() } {
                    ACCESS_DENIED | PIPE_BUSY => Ok(None),
                    _ => Err(std::io::Error::last_os_error()),
                };
            }
            Ok(Some(Server(pipe)))
        }

        /// Wait for a caller and hand it to `serve`; the caller is let go afterwards.
        pub fn next(&self, serve: impl FnOnce(&mut std::fs::File)) {
            // SAFETY: the server's own handle; a caller already there is as good as one arriving
            let connected = unsafe { ConnectNamedPipe(self.0, std::ptr::null_mut()) != 0 || GetLastError() == PIPE_CONNECTED };
            if connected {
                // SAFETY: the handle stays the server's: the File is never dropped
                let mut file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(self.0.cast()) });
                serve(&mut file);
                // SAFETY: the server's own handle
                unsafe { FlushFileBuffers(self.0) };
            }
            // SAFETY: the server's own handle
            unsafe { DisconnectNamedPipe(self.0) };
        }
    }
}

/// Send `command` to a running silli-gui; false when none answers.
#[cfg(windows)]
pub fn send(command: Command) -> bool {
    let Ok(mut stream) = std::fs::OpenOptions::new().read(true).write(true).open(socket_path()) else {
        return false;
    };
    if writeln!(stream, "{}", command.line()).is_err() {
        return false;
    }
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).is_ok() && answer.trim() == "ok"
}

/// This process is the one silli-gui: the pipe made, its commands from now on sent to `sink`.
/// `None` when another silli-gui already has it.
#[cfg(windows)]
pub fn claim(sink: impl Fn(Command) -> bool + Send + 'static) -> std::io::Result<Option<()>> {
    let Some(server) = pipe::Server::claim(&socket_path())? else {
        return Ok(None);
    };
    std::thread::Builder::new().name("silli-gui-control".into()).spawn(move || {
        loop {
            server.next(|stream| {
                let mut line = String::new();
                if BufReader::new(&*stream).read_line(&mut line).is_err() {
                    return;
                }
                let answer = match Command::parse(&line) {
                    Some(command) if sink(command) => "ok",
                    Some(_) => "gone",
                    None => "unknown command",
                };
                let _ = writeln!(stream, "{answer}");
            });
        }
    })?;
    Ok(Some(()))
}
