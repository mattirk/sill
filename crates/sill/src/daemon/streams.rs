//! One-use byte streams. File descriptors remain on the daemon's host. A token is claimed once
//! within [`TOKEN_SECONDS`]; at most [`MAX_PENDING`] wait at a time, and expired ones are
//! dropped on the next token and by the daemon's minute sweep, never by a thread of their own.
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use anyhow::{Context, Result, bail};

const TOKEN_SECONDS: u64 = 60;
const MAX_PENDING: usize = 64;

enum Endpoint { Socket(UnixStream), Terminal(String) }
struct Pending { endpoint: Endpoint, expires: Instant }
fn state() -> &'static Mutex<HashMap<String, Pending>> { static S: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new(); S.get_or_init(Default::default) }

fn token() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
/// Drop tokens nobody claimed in time (and the sockets they hold).
pub fn reap() { state().lock().unwrap().retain(|_, p| p.expires > Instant::now()); }
fn add(endpoint: Endpoint) -> Result<String> {
    let token = token()?;
    let mut s = state().lock().unwrap();
    s.retain(|_, p| p.expires > Instant::now());
    if s.len() >= MAX_PENDING {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Capacity, None, "too many unclaimed streams"));
    }
    s.insert(token.clone(), Pending { endpoint, expires: Instant::now() + Duration::from_secs(TOKEN_SECONDS) });
    Ok(token)
}

pub fn terminal(id: &str) -> Result<String> { add(Endpoint::Terminal(id.into())) }
pub fn port(session: &str, port: u16) -> Result<String> {
    let socket = sill_core::shim::port_stream(&sill_core::session::socket_path(session)?, port)?;
    add(Endpoint::Socket(socket))
}

pub fn socket(socket: UnixStream) -> Result<String> { add(Endpoint::Socket(socket)) }
pub fn discard(token: &str) { state().lock().unwrap().remove(token); }

pub fn claim(token: &str, mut reader: BufReader<UnixStream>) -> Result<()> {
    let pending = state().lock().unwrap().remove(token).context("unknown or already used stream token")?;
    if pending.expires < Instant::now() { bail!("stream token expired"); }
    match pending.endpoint {
        Endpoint::Terminal(id) => super::terminals::serve(&id, reader),
        Endpoint::Socket(mut endpoint) => {
            let mut writer = reader.get_ref().try_clone()?;
            let socket = reader.get_ref().try_clone()?;
            let mut input = endpoint.try_clone()?;
            let copy = std::thread::spawn(move || { let _ = std::io::copy(&mut reader, &mut input); let _ = input.shutdown(std::net::Shutdown::Write); });
            let _ = std::io::copy(&mut endpoint, &mut writer);
            let _ = socket.shutdown(std::net::Shutdown::Both);
            let _ = copy.join();
            Ok(())
        },
    }
}
