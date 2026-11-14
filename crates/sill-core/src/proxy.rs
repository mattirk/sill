//! The session's network proxy (SPEC §4.8). Listens on a Unix socket on the host; inside the jail the
//! shim forwards `127.0.0.1:<port>` to it, and the session environment points `HTTP_PROXY`/`HTTPS_PROXY`
//! at that port. The jail has no other route out.
//!
//! Supports HTTP `CONNECT` (what every TLS client uses) and absolute-form plain HTTP requests. Every
//! connection attempt is decided by [`Rules`] and logged with the rule that decided it. Host-local
//! destinations (loopback, link-local, private ranges) are refused in every mode unless listed in
//! `allow_local`; names are resolved first and every resolved address is checked, so a public name
//! pointing at a private address is refused too.
//!
//! Plain std threads, two per connection; API traffic doesn't need more.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::policy::{NetMode, Network};

#[derive(Debug, Clone)]
pub struct Rules {
    pub mode: NetMode,
    pub allow: Vec<String>,
    pub allow_local: Vec<String>,
}

impl From<&Network> for Rules {
    fn from(n: &Network) -> Self {
        Rules { mode: n.mode, allow: n.allow.clone(), allow_local: n.allow_local.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(&'static str),
    Deny(String),
}

/// `*.example.com` matches any subdomain (not the apex); `*` matches everything; otherwise exact,
/// case-insensitive.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host.len() > suffix.len() && host.ends_with(suffix) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.';
    }
    host == pattern
}

pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_local(IpAddr::V4(v4)))
        }
    }
}

impl Rules {
    pub fn decide(&self, host: &str, addrs: &[IpAddr]) -> Decision {
        let local = host.parse::<IpAddr>().map(is_local).unwrap_or(false) || addrs.iter().any(|a| is_local(*a));
        if local {
            return if self.allow_local.iter().any(|p| host_matches(p, host)) {
                Decision::Allow("allow_local")
            } else {
                Decision::Deny("host-local destination".into())
            };
        }
        match self.mode {
            NetMode::None => Decision::Deny("network = none".into()),
            NetMode::All => Decision::Allow("all"),
            NetMode::Allow => {
                if self.allow.iter().any(|p| host_matches(p, host)) {
                    Decision::Allow("allow-list")
                } else {
                    Decision::Deny("not in allow-list".into())
                }
            }
        }
    }
}

/// Serve forever on an already-bound listener.
pub fn serve(listener: UnixListener, rules: Rules, log: impl Fn(&str) + Send + Sync + 'static) -> Result<()> {
    let rules = std::sync::Arc::new(rules);
    let log = std::sync::Arc::new(log);
    for conn in listener.incoming() {
        let conn = conn.context("accept")?;
        let rules = rules.clone();
        let log = log.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &rules, &*log) {
                log(&format!("error: {e:#}"));
            }
        });
    }
    Ok(())
}

fn handle(mut client: UnixStream, rules: &Rules, log: &dyn Fn(&str)) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
    }
    let mut parts = request_line.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m.to_string(), t.to_string()),
        _ => bail!("bad request line {request_line:?}"),
    };

    let (host, port, origin_form) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(&target, 443)?;
        (h, p, None)
    } else {
        // absolute-form: GET http://host[:port]/path
        let rest = target.strip_prefix("http://").context("only absolute-form http:// requests are proxied")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (h, p) = split_host_port(hostport, 80)?;
        (h, p, Some(path.to_string()))
    };

    let addrs: Vec<IpAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.map(|s| s.ip()).collect(),
        Err(e) => {
            log(&format!("deny {host}:{port} (resolve: {e})"));
            respond(&mut client, "502 Bad Gateway", "sill: cannot resolve host")?;
            return Ok(());
        }
    };
    match rules.decide(&host, &addrs) {
        Decision::Deny(why) => {
            log(&format!("deny {host}:{port} ({why})"));
            respond(&mut client, "403 Forbidden", &format!("sill: {why}"))?;
            return Ok(());
        }
        Decision::Allow(rule) => log(&format!("allow {host}:{port} ({rule})")),
    }

    let mut upstream = TcpStream::connect_timeout(
        &std::net::SocketAddr::new(addrs[0], port),
        Duration::from_secs(20),
    )
    .with_context(|| format!("connect {host}:{port}"))?;
    client.set_read_timeout(None)?;

    if let Some(path) = origin_form {
        // forward the request to the origin, one request per connection
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        for h in &headers {
            let lower = h.to_ascii_lowercase();
            if lower.starts_with("proxy-connection:") || lower.starts_with("connection:") || lower.starts_with("keep-alive:") {
                continue;
            }
            req.push_str(h);
        }
        req.push_str("Connection: close\r\n\r\n");
        upstream.write_all(req.as_bytes())?;
    } else {
        client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    }

    // buffered bytes the reader already consumed belong to the tunnel
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() {
        upstream.write_all(&buffered)?;
    }
    pump(client, upstream);
    Ok(())
}

fn split_host_port(s: &str, default_port: u16) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // [v6]:port
        let (h, p) = rest.split_once(']').context("bad IPv6 host")?;
        let port = p.strip_prefix(':').map(|p| p.parse()).transpose()?.unwrap_or(default_port);
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Ok((h.to_string(), p.parse().context("bad port")?)),
        _ => Ok((s.to_string(), default_port)),
    }
}

fn respond(client: &mut UnixStream, status: &str, body: &str) -> std::io::Result<()> {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(msg.as_bytes())?;
    client.shutdown(Shutdown::Both)
}

/// Copy both directions until both sides are done.
fn pump(client: UnixStream, upstream: TcpStream) {
    let mut c2u = (client.try_clone().ok(), upstream.try_clone().ok());
    let t = std::thread::spawn(move || {
        if let (Some(mut c), Some(mut u)) = (c2u.0.take(), c2u.1.take()) {
            let _ = std::io::copy(&mut c, &mut u);
            let _ = u.shutdown(Shutdown::Write);
        }
    });
    let (mut u, mut c) = (upstream, client);
    let _ = std::io::copy(&mut u, &mut c);
    let _ = c.shutdown(Shutdown::Write);
    let _ = t.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(host_matches("*.anthropic.com", "api.anthropic.com"));
        assert!(!host_matches("*.anthropic.com", "anthropic.com"));
        assert!(!host_matches("*.anthropic.com", "evilanthropic.com"));
        assert!(host_matches("api.anthropic.com", "API.anthropic.com"));
        assert!(host_matches("*", "anything"));
    }

    #[test]
    fn local_refused_even_when_all() {
        let r = Rules { mode: NetMode::All, allow: vec![], allow_local: vec![] };
        assert!(matches!(r.decide("localhost", &["127.0.0.1".parse().unwrap()]), Decision::Deny(_)));
        assert!(matches!(r.decide("10.0.0.5", &[]), Decision::Deny(_)));
        assert!(matches!(r.decide("example.com", &["93.184.216.34".parse().unwrap()]), Decision::Allow(_)));
        let r = Rules { mode: NetMode::Allow, allow: vec!["*.anthropic.com".into()], allow_local: vec!["localhost".into()] };
        assert!(matches!(r.decide("api.anthropic.com", &["1.2.3.4".parse().unwrap()]), Decision::Allow(_)));
        assert!(matches!(r.decide("github.com", &["1.2.3.4".parse().unwrap()]), Decision::Deny(_)));
        assert!(matches!(r.decide("localhost", &["127.0.0.1".parse().unwrap()]), Decision::Allow("allow_local")));
        // public name resolving to a private address: refused
        assert!(matches!(r.decide("api.anthropic.com", &["192.168.1.1".parse().unwrap()]), Decision::Deny(_)));
    }
}
