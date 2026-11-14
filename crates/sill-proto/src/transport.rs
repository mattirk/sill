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
}

fn default_binary() -> String { "sill".into() }

fn config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))).context("HOME is not set")?;
    Ok(base.join("sill/boxes.json"))
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
    let Some(name) = std::env::var_os("SILL_BOX") else { return Ok(None) };
    let name = name.to_str().context("SILL_BOX is not UTF-8")?;
    if name.is_empty() || name == "local" { return Ok(None); }
    list()?.remove(name).map(Some).with_context(|| format!("unknown sill box {name:?}; `sill box list` lists saved boxes"))
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
    if all.remove(name).is_none() { bail!("unknown sill box {name:?}"); }
    write(&all)
}

fn write(all: &BTreeMap<String, BoxTarget>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension(format!("json.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let result = (|| { file.write_all(&serde_json::to_vec_pretty(all)?)?; file.sync_all()?; std::fs::rename(&temp, &path)?; Ok(()) })();
    if result.is_err() { let _ = std::fs::remove_file(temp); }
    result
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "local" || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        bail!("box name must contain letters, digits, '.', '-' or '_' and cannot be 'local'");
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_control() || c.is_whitespace()) { bail!("invalid SSH destination"); }
    Ok(())
}

fn validate_binary(binary: &str) -> Result<()> {
    if binary.is_empty() || binary.chars().any(char::is_control) { bail!("invalid remote sill executable path"); }
    Ok(())
}

fn quote(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

pub fn binary_command(binary: &str) -> Result<String> {
    validate_binary(binary)?;
    Ok(match binary.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(binary),
    })
}

pub fn ssh_base(host: &str) -> Result<Command> {
    ssh_base_mode(host, false)
}

fn ssh_base_mode(host: &str, interactive: bool) -> Result<Command> {
    validate_host(host)?;
    let dir = crate::runtime_dir()?.join("ssh");
    std::fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let mut command = Command::new("ssh");
    command.args(["-T", "-o", if interactive { "BatchMode=no" } else { "BatchMode=yes" }, "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "ControlMaster=auto", "-o", "ControlPersist=60"])
        .arg("-o").arg(format!("ControlPath={}/%C", dir.display())).arg("--").arg(host);
    Ok(command)
}

/// Authenticate before starting the protocol timeout. OpenSSH prompts on /dev/tty,
/// and ControlPersist keeps the authenticated connection available to protocol clients.
pub fn authenticate(host: &str) -> Result<()> {
    let status = ssh_base_mode(host, true)?.arg("true")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status().context("starting SSH authentication")?;
    if !status.success() {
        bail!("SSH authentication/connection to {host} failed ({status}); see SSH's message above. Check the login, host key and network; no Sill protocol connection was made");
    }
    Ok(())
}

pub fn ssh_command(host: &str, binary: &str) -> Result<Command> {
    let mut command = ssh_base(host)?;
    command.arg(format!("exec {} daemon --stdio", binary_command(binary)?));
    Ok(command)
}

/// Push an explicitly selected headless binary atomically to the person's account on the box.
/// SSH handles host-key verification; the binary's bytes travel on stdin, never in shell text.
pub fn install(host: &str, source: &Path) -> Result<()> {
    // Check both before touching the box: the daemon stages sill-shim beside its own binary
    // and cannot launch a jail without it.
    let shim = source.with_file_name("sill-shim");
    let daemon_file = std::fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let shim_file = std::fs::File::open(&shim).with_context(|| format!("{} must be beside the headless sill binary", shim.display()))?;
    for (file, name) in [(shim_file, "sill-shim"), (daemon_file, "sill")] {
        let script = format!("set -eu; mkdir -p \"$HOME/.local/bin\"; sill_tmp=$(mktemp \"$HOME/.local/bin/.{name}.XXXXXX\"); trap 'rm -f \"$sill_tmp\"' EXIT; cat > \"$sill_tmp\"; chmod 755 \"$sill_tmp\"; mv -f \"$sill_tmp\" \"$HOME/.local/bin/{name}\"");
        let status = ssh_base(host)?.arg(script).stdin(file).status().context("copying sill to the box over ssh")?;
        if !status.success() { bail!("installing {name} on {host} failed ({status})"); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_authentication_reuses_batch_connection_path() {
        let interactive = ssh_base_mode("example.test", true).unwrap();
        let batch = ssh_base("example.test").unwrap();
        let args = |cmd: &Command| cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let interactive = args(&interactive);
        let batch = args(&batch);
        assert!(interactive.contains(&"BatchMode=no".into()));
        assert!(batch.contains(&"BatchMode=yes".into()));
        assert_eq!(interactive.iter().find(|a| a.starts_with("ControlPath=")),
                   batch.iter().find(|a| a.starts_with("ControlPath=")));
        assert!(batch.contains(&"ControlPersist=60".into()));
    }

    #[test]
    fn remote_executable_is_one_shell_word() {
        assert_eq!(binary_command("~/a'b/sill").unwrap(), "\"$HOME\"/'a'\\''b/sill'");
        assert_eq!(binary_command("/tmp/a; touch /tmp/no").unwrap(), "'/tmp/a; touch /tmp/no'");
        assert!(ssh_base("-oProxyCommand=bad").is_err());
        assert!(ssh_base("host\ncommand").is_err());
        assert!(validate_name("local").is_err());
    }
    #[test]
    fn closing_pipe_input_preserves_the_response_direction() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        let mut child = Command::new("/bin/sh").args(["-c", "cat; printf done"])
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
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
        match self.0.lock().unwrap().as_mut() { Some(input) => input.flush(), None => Ok(()) }
    }
}

pub fn open_stream(token: &str) -> Result<RawStream> {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    if token.is_empty() || token.chars().any(char::is_control) { bail!("invalid stream token"); }
    let handshake = serde_json::to_vec(&serde_json::json!({"_sill_stream":token}))?;
    let mut stream = if let Some(target) = selected()? {
        let mut child = ssh_command(&target.host, &target.binary)?.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).spawn().context("opening SSH stream")?;
        let reader = child.stdout.take().context("ssh stdout")?;
        let writer = Arc::new(Mutex::new(Some(child.stdin.take().context("ssh stdin")?)));
        let half = writer.clone();
        let child = Arc::new(Mutex::new(child));
        RawStream { reader: Box::new(reader), writer: Box::new(PipeInput(writer)), shutdown_write: Arc::new(move || { half.lock().unwrap().take(); }), interrupt: Arc::new(move || {
            if let Ok(mut child) = child.lock() { let _ = child.kill(); let _ = child.wait(); }
        }) }
    } else {
        let socket = std::os::unix::net::UnixStream::connect(crate::socket_path()?).context("opening daemon byte stream")?;
        let close = socket.try_clone()?;
        let half = socket.try_clone()?;
        RawStream { reader: Box::new(socket.try_clone()?), writer: Box::new(socket), shutdown_write: Arc::new(move || { let _ = half.shutdown(std::net::Shutdown::Write); }), interrupt: Arc::new(move || { let _ = close.shutdown(std::net::Shutdown::Both); }) }
    };
    if let Err(error) = stream.writer.write_all(&handshake).and_then(|()| stream.writer.write_all(b"\n")).and_then(|()| stream.writer.flush()) {
        (stream.interrupt)();
        return Err(error).context("opening the daemon stream");
    }
    Ok(stream)
}
