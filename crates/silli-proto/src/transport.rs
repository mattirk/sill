//! Local connection preferences. These belong to the frontend machine, never to a box's store.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxTarget {
    pub host: String,
    #[serde(default = "default_binary")]
    pub binary: String,
    /// An SSH config file of the box's own (`ssh -F`), in place of the person's: the Mac's own VM,
    /// reached over a local socket with a key silli made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config: Option<PathBuf>,
}

/// The box this process talks to, chosen by the process itself rather than by `SILLI_BOX` (the Mac
/// window's own VM), with the name it shows.
static CHOSEN: std::sync::OnceLock<(String, BoxTarget)> = std::sync::OnceLock::new();

/// How this process opens a byte pipe to its box's `silli daemon --stdio`, when the box is reached
/// neither over SSH nor on a local socket: a Windows window's own VM, over the VM's host–guest
/// socket (docs/silli-windows.md, section 6.2).
pub type Opener = std::sync::Arc<dyn Fn() -> Result<RawStream> + Send + Sync>;

static PIPE: std::sync::OnceLock<(String, Opener)> = std::sync::OnceLock::new();

/// This process talks to the box named `name` through pipes `open` makes, from now on; once.
pub fn choose_pipe(name: &str, open: Opener) {
    let _ = PIPE.set((name.to_string(), open));
}

/// The chosen pipe, when the box is reached by one.
pub(crate) fn pipe() -> Option<&'static Opener> {
    PIPE.get().map(|(_, open)| open)
}

/// The daemon is this machine's own, on its local socket: no box was chosen in any way.
pub fn is_local() -> bool {
    PIPE.get().is_none() && matches!(selected(), Ok(None))
}

/// This process talks to `target` (named `name`) from now on; once.
pub fn choose(name: &str, target: BoxTarget) {
    let _ = CHOSEN.set((name.to_string(), target));
}

/// The name of the box this process talks to, `None` for the local daemon.
pub fn selected_name() -> Option<String> {
    if let Some((name, _)) = PIPE.get() {
        return Some(name.clone());
    }
    if let Some((name, _)) = CHOSEN.get() {
        return Some(name.clone());
    }
    std::env::var("SILLI_BOX").ok().filter(|name| !name.is_empty() && name != "local")
}

/// `ssh`, with the chosen box's own config file when `host` is that box.
pub(crate) fn ssh_program(host: &str) -> Command {
    let mut command = Command::new("ssh");
    if let Some((_, target)) = CHOSEN.get()
        && target.host == host
        && let Some(config) = &target.ssh_config
    {
        command.arg("-F").arg(config);
    }
    command
}

fn default_binary() -> String {
    "silli".into()
}

fn config_path() -> Result<PathBuf> {
    Ok(crate::config_dir()?.join("boxes.json"))
}

pub fn list() -> Result<BTreeMap<String, BoxTarget>> {
    let path = config_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn selected() -> Result<Option<BoxTarget>> {
    if let Some((_, target)) = CHOSEN.get() {
        return Ok(Some(target.clone()));
    }
    let Some(name) = std::env::var_os("SILLI_BOX") else { return Ok(None) };
    let name = name.to_str().context("SILLI_BOX is not UTF-8")?;
    if name.is_empty() || name == "local" {
        return Ok(None);
    }
    list()?.remove(name).map(Some).with_context(|| format!("unknown silli box {name:?}; `silli box list` lists saved boxes"))
}

pub fn save(name: &str, target: BoxTarget) -> Result<()> {
    validate_name(name)?;
    validate_host(&target.host)?;
    validate_binary(&target.binary)?;
    let mut all = list()?;
    all.insert(name.to_string(), target);
    write(&all)
}

pub fn remove(name: &str) -> Result<()> {
    let mut all = list()?;
    if all.remove(name).is_none() {
        bail!("unknown silli box {name:?}");
    }
    write(&all)
}

fn write(all: &BTreeMap<String, BoxTarget>) -> Result<()> {
    use std::io::Write;
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension(format!("json.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // the owner's alone; on Windows the account's settings folder already is
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(all)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "local" || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        bail!("box name must contain letters, digits, '.', '-' or '_' and cannot be 'local'");
    }
    Ok(())
}

/// The name a box gets when none is given: the host without its user, with anything a name
/// cannot hold (an IPv6 address's colons, an `ssh://` URI's slashes) as `-`.
pub fn default_name(host: &str) -> String {
    let host = host.strip_prefix("ssh://").unwrap_or(host);
    let host = host.rsplit('@').next().unwrap_or(host);
    let name: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() || name == "local" { "box".into() } else { name }
}

/// Ask the daemon on `host` for its status, then save the box as `name`: what `silli box add` and
/// the window's Add box do once SSH is signed in (or can log in without asking).
pub fn verify_and_save(name: &str, host: &str, binary: &str) -> Result<crate::DaemonStatus> {
    validate_name(name)?;
    validate_host(host)?;
    validate_binary(binary)?;
    let client = crate::Client::ssh(host, binary).context("checking the box over SSH; verify SSH access and the remote silli binary path")?;
    let status: crate::DaemonStatus = client.call(crate::method::DAEMON_STATUS, crate::Empty::default())?;
    drop(client);
    save(name, BoxTarget { host: host.to_string(), binary: binary.to_string(), ssh_config: None })?;
    Ok(status)
}

pub(crate) fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("invalid SSH destination");
    }
    Ok(())
}

fn validate_binary(binary: &str) -> Result<()> {
    if binary.is_empty() || binary.chars().any(char::is_control) {
        bail!("invalid remote silli executable path");
    }
    Ok(())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn binary_command(binary: &str) -> Result<String> {
    validate_binary(binary)?;
    Ok(match binary.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(binary),
    })
}

/// An `ssh` that rides the box's shared connection (`boxlink`) and never becomes one: it cannot
/// prompt, and it gives up on a dead network within a minute.
pub fn ssh_base(host: &str) -> Result<Command> {
    validate_host(host)?;
    let mut command = ssh_program(host);
    command
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "ControlMaster=no"])
        .arg("-o")
        .arg(crate::boxlink::control_path()?)
        .arg("--")
        .arg(host);
    Ok(command)
}

pub fn ssh_command(host: &str, binary: &str) -> Result<Command> {
    let mut command = ssh_base(host)?;
    command.arg(format!("exec {} daemon --stdio", binary_command(binary)?));
    Ok(command)
}

/// Push an explicitly selected headless binary atomically to the person's account on the box.
/// SSH handles host-key verification; the binary's bytes travel on stdin, never in shell text.
pub fn install(host: &str, source: &Path) -> Result<()> {
    // Check both before touching the box: the daemon stages silli-shim beside its own binary
    // and cannot launch a jail without it.
    let shim = source.with_file_name("silli-shim");
    let daemon_file = std::fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let shim_file = std::fs::File::open(&shim).with_context(|| format!("{} must be beside the headless silli binary", shim.display()))?;
    crate::boxlink::ensure(host)?;
    for (file, name) in [(shim_file, "silli-shim"), (daemon_file, "silli")] {
        let script = format!(
            "set -eu; mkdir -p \"$HOME/.local/bin\"; silli_tmp=$(mktemp \"$HOME/.local/bin/.{name}.XXXXXX\"); trap 'rm -f \"$silli_tmp\"' EXIT; cat > \"$silli_tmp\"; chmod 755 \"$silli_tmp\"; mv -f \"$silli_tmp\" \"$HOME/.local/bin/{name}\""
        );
        let status = ssh_base(host)?.arg(script).stdin(file).status().context("copying silli to the box over ssh")?;
        if !status.success() {
            bail!("installing {name} on {host} failed ({status})");
        }
    }
    Ok(())
}

/// An unframed byte connection, used for terminals and ports after its one-time token line.
/// Keep this value alive while its halves are used; `interrupt` closes both directions.
pub struct RawStream {
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub interrupt: std::sync::Arc<dyn Fn() + Send + Sync>,
    pub shutdown_write: std::sync::Arc<dyn Fn() + Send + Sync>,
}

struct PipeInput(std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>);
impl std::io::Write for PipeInput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        self.0.lock().unwrap().as_mut().ok_or_else(|| Error::new(ErrorKind::BrokenPipe, "stream input closed"))?.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock().unwrap().as_mut() {
            Some(input) => input.flush(),
            None => Ok(()),
        }
    }
}

pub fn open_stream(token: &str) -> Result<RawStream> {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    if token.is_empty() || token.chars().any(char::is_control) {
        bail!("invalid stream token");
    }
    let handshake = serde_json::to_vec(&serde_json::json!({"_silli_stream":token}))?;
    let mut stream = if let Some(open) = pipe() {
        open()?
    } else if let Some(target) = selected()? {
        crate::boxlink::ensure(&target.host)?;
        let mut child = ssh_command(&target.host, &target.binary)?
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("opening SSH stream")?;
        let reader = child.stdout.take().context("ssh stdout")?;
        let writer = Arc::new(Mutex::new(Some(child.stdin.take().context("ssh stdin")?)));
        let half = writer.clone();
        let child = Arc::new(Mutex::new(child));
        RawStream {
            reader: Box::new(reader),
            writer: Box::new(PipeInput(writer)),
            shutdown_write: Arc::new(move || {
                half.lock().unwrap().take();
            }),
            interrupt: Arc::new(move || {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }),
        }
    } else {
        #[cfg(not(unix))]
        bail!("no silli daemon runs on this system itself: a box must be chosen");
        #[cfg(unix)]
        {
            let socket = std::os::unix::net::UnixStream::connect(crate::socket_path()?).context("opening daemon byte stream")?;
            let close = socket.try_clone()?;
            let half = socket.try_clone()?;
            RawStream {
                reader: Box::new(socket.try_clone()?),
                writer: Box::new(socket),
                shutdown_write: Arc::new(move || {
                    let _ = half.shutdown(std::net::Shutdown::Write);
                }),
                interrupt: Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            }
        }
    };
    if let Err(error) = stream.writer.write_all(&handshake).and_then(|()| stream.writer.write_all(b"\n")).and_then(|()| stream.writer.flush()) {
        (stream.interrupt)();
        return Err(error).context("opening the daemon stream");
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_box_is_named_after_its_host() {
        assert_eq!(default_name("matti@192.168.1.50"), "192.168.1.50");
        assert_eq!(default_name("my-server"), "my-server");
        assert_eq!(default_name("ssh://me@host.example:2222"), "host.example-2222");
        assert_eq!(default_name("me@fe80::1"), "fe80--1");
        assert_eq!(default_name("local"), "box");
        for host in ["matti@192.168.1.50", "ssh://me@host.example:2222", "me@fe80::1", "local"] {
            validate_name(&default_name(host)).unwrap();
        }
    }
    #[test]
    fn session_connections_ride_the_shared_connection_and_never_prompt() {
        let args = ssh_base("example.test").unwrap().get_args().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert!(args.contains(&"BatchMode=yes".into()));
        assert!(args.contains(&"ControlMaster=no".into()), "only boxlink starts a master");
        assert!(args.contains(&"ConnectTimeout=10".into()));
        assert_eq!(args.iter().find(|a| a.starts_with("ControlPath=")), Some(&crate::boxlink::control_path().unwrap()));
    }

    #[test]
    fn remote_executable_is_one_shell_word() {
        assert_eq!(binary_command("~/a'b/silli").unwrap(), "\"$HOME\"/'a'\\''b/silli'");
        assert_eq!(binary_command("/tmp/a; touch /tmp/no").unwrap(), "'/tmp/a; touch /tmp/no'");
        assert!(ssh_base("-oProxyCommand=bad").is_err());
        assert!(ssh_base("host\ncommand").is_err());
        assert!(validate_name("local").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn closing_pipe_input_preserves_the_response_direction() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        let mut child =
            Command::new("/bin/sh").args(["-c", "cat; printf done"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let input = Arc::new(Mutex::new(child.stdin.take()));
        let mut writer = PipeInput(input.clone());
        writer.write_all(b"request").unwrap();
        input.lock().unwrap().take(); // the RawStream shutdown_write callback
        let mut output = String::new();
        child.stdout.take().unwrap().read_to_string(&mut output).unwrap();
        assert_eq!(output, "requestdone");
        assert_eq!(writer.write(b"late").unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert!(child.wait().unwrap().success());
    }
}
