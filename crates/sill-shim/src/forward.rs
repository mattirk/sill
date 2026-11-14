//! The jail's side of the session proxy (SPEC §4.8): `127.0.0.1:3128` inside the jail, bridged to
//! the proxy's Unix socket on the host — the jail's network namespace has only `lo`, a socket path
//! crosses it. Inside the jail `127.0.0.1` is two places at once: the agent's own servers (jail
//! loopback) and, through the proxy and `allow_local`, the host's. So the bridge looks at each
//! request first: a loopback target with a listener in the jail is served locally — `CONNECT` gets
//! its `200`, an absolute-form request its origin-form line — and everything else goes to the proxy
//! as it came. The agent's own server wins when it exists; host services still go through policy.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

/// Serve forever on `listen`, each connection on its own thread.
pub fn serve(listen: &str, socket: PathBuf) -> Result<()> {
    let l = TcpListener::bind(listen).with_context(|| format!("listen on {listen}"))?;
    for conn in l.incoming() {
        let Ok(conn) = conn else { continue };
        let socket = socket.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &socket) {
                eprintln!("sill-shim: forwarder: {e:#}");
            }
        });
    }
    Ok(())
}

/// What a request asks for: the target and, for an absolute-form request, the origin-form path.
#[derive(Debug, PartialEq)]
struct Target {
    method: String,
    host: String,
    port: u16,
    /// `Some(path)` for `GET http://host/path`, `None` for `CONNECT host:port`.
    path: Option<String>,
}

/// Parse a proxy request line. Anything else (origin-form, `https://`) is `None`: the proxy says no.
fn parse_target(request_line: &str) -> Option<Target> {
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443)?;
        return Some(Target {
            method: method.into(),
            host,
            port,
            path: None,
        });
    }
    let rest = target.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(hostport, 80)?;
    Some(Target {
        method: method.into(),
        host,
        port,
        path: Some(path.into()),
    })
}

fn split_host_port(s: &str, default: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // [v6]:port
        let (h, p) = rest.split_once(']')?;
        let port = match p.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default,
        };
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default)),
    }
}

/// `localhost` or a loopback literal: the jail's own `lo` — no resolver, the proxy owns names.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Ports with a TCP listener in this network namespace, from `/proc/net/tcp{,6}` text.
fn listening_ports(table: &str) -> HashSet<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let local = f.nth(1)?;
            let state = f.nth(1)?;
            if state != "0A" {
                return None; // LISTEN
            }
            u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()
        })
        .collect()
}

fn jail_listens_on(port: u16) -> bool {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|t| listening_ports(&t).contains(&port))
}

/// A two-way byte pump needs a second handle of each end; both stream kinds give one.
trait Stream: Read + Write + Send + 'static {
    fn dup(&self) -> std::io::Result<Self>
    where
        Self: Sized;
    fn close_write(&self);
}

impl Stream for TcpStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

impl Stream for UnixStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

fn pump(client: TcpStream, upstream: impl Stream) {
    let (Ok(mut c2), Ok(mut u2)) = (client.dup(), upstream.dup()) else {
        return;
    };
    let (mut c, mut u) = (client, upstream);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c2, &mut u2);
        u2.close_write();
    });
    let _ = std::io::copy(&mut u, &mut c);
    c.close_write();
    let _ = t.join();
}

fn handle(mut client: TcpStream, socket: &PathBuf) -> Result<()> {
    // the head, kept as it came: it goes to the proxy byte for byte
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut head: Vec<u8> = Vec::new();
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    head.extend_from_slice(request_line.as_bytes());
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head.extend_from_slice(line.as_bytes());
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
    }
    client.set_read_timeout(None)?;
    let buffered = reader.buffer().to_vec();

    let local =
        parse_target(&request_line).filter(|t| is_loopback(&t.host) && jail_listens_on(t.port));
    let Some(t) = local else {
        let mut unix =
            UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
        unix.write_all(&head)?;
        unix.write_all(&buffered)?;
        pump(client, unix);
        return Ok(());
    };

    let addr = std::net::SocketAddr::new(IpAddr::from([127, 0, 0, 1]), t.port);
    let mut up = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .or_else(|_| {
            TcpStream::connect_timeout(
                &std::net::SocketAddr::new(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]), t.port),
                Duration::from_secs(5),
            )
        })
        .with_context(|| format!("connect jail {}:{}", t.host, t.port))?;
    match &t.path {
        None => client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?,
        Some(path) => {
            // origin-form, one request per connection, as the proxy does
            let mut req = format!("{} {path} HTTP/1.1\r\n", t.method);
            for h in &headers {
                let lower = h.to_ascii_lowercase();
                if lower.starts_with("proxy-connection:")
                    || lower.starts_with("connection:")
                    || lower.starts_with("keep-alive:")
                {
                    continue;
                }
                req.push_str(h);
            }
            req.push_str("Connection: close\r\n\r\n");
            up.write_all(req.as_bytes())?;
        }
    }
    if !buffered.is_empty() {
        up.write_all(&buffered)?;
    }
    pump(client, up);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("CONNECT 127.0.0.1:8080 HTTP/1.1\r\n"),
            Some(Target {
                method: "CONNECT".into(),
                host: "127.0.0.1".into(),
                port: 8080,
                path: None
            })
        );
        assert_eq!(
            parse_target("GET http://localhost:3000/api?x=1 HTTP/1.1\r\n"),
            Some(Target {
                method: "GET".into(),
                host: "localhost".into(),
                port: 3000,
                path: Some("/api?x=1".into())
            })
        );
        assert_eq!(
            parse_target("GET http://example.com HTTP/1.1\r\n").map(|t| (t.port, t.path)),
            Some((80, Some("/".into())))
        );
        assert_eq!(
            parse_target("CONNECT [::1]:8443 HTTP/1.1").map(|t| (t.host, t.port)),
            Some(("::1".into(), 8443))
        );
        assert_eq!(
            parse_target("CONNECT api.example.com HTTP/1.1").map(|t| t.port),
            Some(443)
        );
        assert_eq!(parse_target("GET /path HTTP/1.1"), None);
        assert_eq!(parse_target("GET https://example.com/ HTTP/1.1"), None);
        assert_eq!(parse_target(""), None);
    }

    #[test]
    fn loopback_names() {
        assert!(
            is_loopback("localhost")
                && is_loopback("LOCALHOST")
                && is_loopback("127.0.0.1")
                && is_loopback("127.1.2.3")
                && is_loopback("::1")
        );
        assert!(!is_loopback("example.com") && !is_loopback("10.0.0.1") && !is_loopback("0.0.0.0"));
    }

    #[test]
    fn listeners_from_proc_net_tcp() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                     0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n\
                     1: 0100007F:0C38 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12346 1 0000000000000000 100 0 0 10 0\n\
                     2: 0100007F:A3F2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 12347 1 0000000000000000 20 4 30 10 -1\n";
        let ports = listening_ports(table);
        assert!(ports.contains(&8080) && ports.contains(&3128));
        assert!(!ports.contains(&41970)); // established, not listening
        assert!(listening_ports("").is_empty());
    }
}
