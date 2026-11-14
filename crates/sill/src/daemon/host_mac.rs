//! sill-mac (docs/sill-mac.md): the Mac operations of a session's Host control, for a daemon that
//! runs in Sill.app's VM. The daemon checks the session's grant and generation as for any host
//! operation, then hands the request to Sill.app on the Mac over the VM's host–guest socket
//! (vsock, the Mac's address 2, port [`PORT`]); Sill.app checks the grant again against its own
//! view of the session before it acts.
//!
//! The Mac only answers a request that carries the secret it gave this daemon at the VM's start:
//! vsock is not confined by a network namespace, so a jailed process could reach the port; the
//! secret is in the daemon's runtime directory, which jails never see.
//!
//! One request per connection: a JSON line out; a JSON line back, followed by `bytes` raw bytes
//! when the reply names some (a picture).
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The Mac side's port, where Sill.app listens on the VM's socket device.
pub const PORT: u32 = 7695;
/// vsock's address of the host (the Mac).
const HOST_CID: u32 = 2;
/// The largest reply picture accepted: a 6K display's PNG is far smaller.
const MAX_PICTURE: u64 = 128 << 20;

fn token_path() -> Result<std::path::PathBuf> {
    Ok(sill_proto::runtime_dir()?.join("mac.token"))
}

/// Sill.app gave this daemon its secret: the daemon runs in Sill.app's VM, and the Mac is there.
pub fn attached() -> bool {
    token_path().is_ok_and(|p| p.is_file())
}

/// A request to the Mac for `session` under Host control `generation`: the reply, and the
/// picture's bytes when there is one.
pub fn request(session: &str, generation: u64, op: &str, args: &Value) -> Result<(Value, Option<Vec<u8>>)> {
    let token = std::fs::read_to_string(token_path()?)
        .context("no Mac attached: this daemon does not run in Sill.app's VM")?;
    let stream = connect().context("Sill.app on the Mac does not answer")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    writeln!(
        writer,
        "{}",
        json!({"token": token.trim(), "session": session, "generation": generation, "op": op, "args": args})
    )?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply: Value = serde_json::from_str(&line)
        .context("the Mac's reply was cut off; the outcome is unknown, do not replay automatically")?;
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    let picture = match reply.get("bytes").and_then(Value::as_u64) {
        Some(n) if n > MAX_PICTURE => bail!("the Mac sent a picture of {n} bytes, over the limit"),
        Some(n) => {
            let mut bytes = vec![0u8; n as usize];
            reader.read_exact(&mut bytes).context("the Mac's picture was cut off")?;
            Some(bytes)
        }
        None => None,
    };
    Ok((reply.get("result").cloned().unwrap_or(Value::Null), picture))
}

/// A stream to the Mac's port over vsock.
fn connect() -> Result<UnixStream> {
    // SAFETY: a plain socket; the address is a fully initialised sockaddr_vm
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        let mut address: libc::sockaddr_vm = std::mem::zeroed();
        address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        address.svm_cid = HOST_CID;
        address.svm_port = PORT;
        let connected = libc::connect(
            fd,
            (&address as *const libc::sockaddr_vm).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if connected != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // a stream socket like any other: std's Unix stream reads and writes it
        Ok(UnixStream::from(owned))
    }
}

/// What `mac_view` accepts: one listing or one picture.
pub fn check_view(args: &Value) -> Result<()> {
    let object = args.as_object().context("mac_view takes a JSON object")?;
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    match keys.as_slice() {
        ["list"] => match object["list"].as_str() {
            Some("displays" | "windows") => Ok(()),
            _ => bail!("list is \"displays\" or \"windows\""),
        },
        ["display"] | ["window"] => {
            object.values().next().and_then(Value::as_u64).context("a display or window is its numeric id")?;
            Ok(())
        }
        _ => bail!("mac_view takes {{\"list\":\"displays\"}}, {{\"list\":\"windows\"}}, {{\"display\":ID}} or {{\"window\":ID}}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn mac_view_takes_one_listing_or_one_picture() {
        for ok in [json!({"list":"displays"}), json!({"list":"windows"}), json!({"display":1}), json!({"window":4242})] {
            assert!(super::check_view(&ok).is_ok(), "{ok}");
        }
        for bad in [
            json!({}),
            json!({"list":"files"}),
            json!({"display":"main"}),
            json!({"display":1,"window":2}),
            json!({"path":"/etc/passwd"}),
            json!([1]),
        ] {
            assert!(super::check_view(&bad).is_err(), "{bad}");
        }
    }
}
