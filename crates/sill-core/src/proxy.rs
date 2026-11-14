//! The session's network proxy (SPEC §4.8). Listens on a Unix socket on the host; inside the jail the
//! shim forwards `127.0.0.1:<port>` to it, and the session environment points `HTTP_PROXY`/`HTTPS_PROXY`
//! at that port. The jail has no other route out.
//!
//! Supports HTTP `CONNECT` (what every TLS client uses) and absolute-form plain HTTP requests. Every
//! connection attempt is decided by [`Rules`] in two stages and logged with the rule that decided it:
//! first on the name and port alone — a literal address is classified at once, a name has to be on a
//! list (or the mode `all`) before it is resolved at all, so nothing in the jail can use the proxy as a
//! DNS oracle or a covert channel (SPEC §4.11 #7); then on every address the name resolved to —
//! host-local destinations (loopback, link-local, private ranges, the reserved blocks) are refused in
//! every mode unless `allow_local` names them, so a public name pointing at a private address is
//! refused too. A pattern grants one port: the scheme's own (443 for `CONNECT`, 80 for plain HTTP)
//! unless it says `host:port` or `host:*` (SPEC §4.11 #9).
//!
//! Plain std threads, two per connection, at most [`MAX_CONNECTIONS`] connections at a time; the
//! request head is read with a size cap; an `accept` error is logged, not fatal. API traffic doesn't
//! need more.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::policy::{NetMode, Network};

/// Connections served at once; the rest get a 503 and go. The jail's unit shares `TasksMax` with
/// the proxy's threads, so a client opening sockets in a loop must not take the shelf down.
pub const MAX_CONNECTIONS: usize = 64;
/// The request line and each header line are read up to this many bytes; a longer one is a 431.
const MAX_LINE: usize = 8 * 1024;
const MAX_HEADERS: usize = 100;
/// Default port of each request form: what a bare pattern grants.
pub const CONNECT_PORT: u16 = 443;
pub const HTTP_PORT: u16 = 80;

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
/// case-insensitive. A port suffix on the pattern is ignored here (see [`pattern_matches`]).
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let (pattern, _) = split_pattern(pattern);
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

/// A host name the shelf may show and offer to allow (security.md S7): an IP literal, or DNS
/// labels of ASCII letters, digits and hyphens — no wildcard, no empty label, nothing a terminal
/// renders as something else. The proxy denies on anything the rules don't match anyway; this is
/// about what gets *written into a button* and, on a click, into the agent's allow-list.
pub fn valid_hostname(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 || host.ends_with('.') {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Multi-label public suffixes (an allow-list entry of `*.co.uk` would be "every site in the UK"):
/// no wildcard offer ends in one of these. A short list of the common ones — the full public
/// suffix list is thousands of lines and changes monthly; a name whose parent is not here still
/// gets a wildcard only when that parent has two labels or more.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "sch.uk", "nhs.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "id.au",
    "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "co.kr", "or.kr", "co.in", "net.in", "org.in",
    "co.nz", "net.nz", "org.nz", "co.za", "org.za", "co.il", "org.il", "co.id", "or.id",
    "com.br", "net.br", "org.br", "com.cn", "net.cn", "org.cn", "com.hk", "com.sg", "com.tw", "com.mx", "com.ar", "com.tr", "com.ua",
    // hosting where every tenant has a sibling name: a wildcard would allow every tenant
    "github.io", "gitlab.io", "herokuapp.com", "vercel.app", "netlify.app", "pages.dev", "workers.dev",
    "web.app", "firebaseapp.com", "azurewebsites.net", "cloudfront.net", "amazonaws.com", "s3.amazonaws.com",
    "appspot.com", "run.app", "fly.dev", "onrender.com", "railway.app", "ngrok.io", "ngrok-free.app", "ngrok.app",
    "trycloudflare.com", "blogspot.com", "wordpress.com", "github.dev", "repl.co", "glitch.me", "surge.sh",
];

/// The wildcard the shelf may offer for a refused name: `*.<parent>` when the name has three
/// labels or more and the parent is not a public suffix (`evil.co.uk` offers nothing, not
/// `*.co.uk`; `api.example.com` offers `*.example.com`). `None` for IP literals.
pub fn wildcard_offer(name: &str) -> Option<String> {
    if !valid_hostname(name) || name.parse::<IpAddr>().is_ok() {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 3 {
        return None;
    }
    let parent = labels[1..].join(".").to_ascii_lowercase();
    if PUBLIC_SUFFIXES.contains(&parent.as_str()) {
        return None;
    }
    Some(format!("*.{parent}"))
}

/// What a pattern says about the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortRule {
    /// Nothing: the request form's own port.
    Default,
    Exact(u16),
    Any,
}

/// `host`, `host:8443`, `host:*`, `[::1]:8080` → the host part and its port rule.
fn split_pattern(p: &str) -> (&str, PortRule) {
    if let Some(rest) = p.strip_prefix('[')
        && let Some((h, tail)) = rest.split_once(']')
    {
        return (h, port_rule(tail.strip_prefix(':')));
    }
    match p.rsplit_once(':') {
        Some((h, tail)) if !h.contains(':') => (h, port_rule(Some(tail))),
        _ => (p, PortRule::Default),
    }
}

fn port_rule(tail: Option<&str>) -> PortRule {
    match tail {
        None | Some("") => PortRule::Default,
        Some("*") => PortRule::Any,
        Some(n) => n.parse().map(PortRule::Exact).unwrap_or(PortRule::Default),
    }
}

/// The pattern grants `host:port` for a request form whose own port is `default_port`.
pub fn pattern_matches(pattern: &str, host: &str, port: u16, default_port: u16) -> bool {
    let (_, rule) = split_pattern(pattern);
    host_matches(pattern, host)
        && match rule {
            PortRule::Default => port == default_port,
            PortRule::Exact(p) => port == p,
            PortRule::Any => true,
        }
}

/// Loopback, private, link-local, CGNAT, multicast and the reserved/unrouted blocks: anything that
/// could be this machine or its neighbours rather than the internet (security.md S14).
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0 // 0.0.0.0/8 "this network"
                || o[0] == 100 && (64..128).contains(&o[1]) // CGNAT 100.64/10
                || o[0] == 192 && o[1] == 0 && o[2] == 0 // 192.0.0.0/24 IETF protocol assignments
                || o[0] == 198 && (o[1] & 0xfe) == 18 // 198.18/15 benchmarking
                || o[0] >= 240 // 240/4 reserved (and 255.255.255.255)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
                || v6.to_ipv4().is_some_and(|v4| is_local(IpAddr::V4(v4))) // ::ffff:a.b.c.d and ::a.b.c.d
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]) && is_local(embedded_v4(s[6], s[7])) // 64:ff9b::/96 NAT64
                || s[0] == 0x2002 && is_local(embedded_v4(s[1], s[2])) // 2002::/16 6to4
        }
    }
}

fn embedded_v4(hi: u16, lo: u16) -> IpAddr {
    IpAddr::V4(std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8))
}

impl Rules {
    /// First stage, before any DNS: the name (or literal address) and the port against the lists.
    /// `allow_local` is checked first so a host-local service the policy names is reachable in
    /// every mode; a literal local address not named there is refused at once; a name is allowed
    /// to be resolved only when the mode or the allow-list says so.
    pub fn decide_name(&self, host: &str, port: u16, default_port: u16) -> Decision {
        if self.allow_local.iter().any(|p| pattern_matches(p, host, port, default_port)) {
            return Decision::Allow("allow_local");
        }
        if let Ok(ip) = host.parse::<IpAddr>()
            && is_local(ip)
        {
            return Decision::Deny("host-local destination".into());
        }
        match self.mode {
            NetMode::None => Decision::Deny("network = none".into()),
            NetMode::All => Decision::Allow("all"),
            NetMode::Allow => {
                if self.allow.iter().any(|p| pattern_matches(p, host, port, default_port)) {
                    Decision::Allow("allow-list")
                } else {
                    Decision::Deny("not in allow-list".into())
                }
            }
        }
    }

    /// Second stage, on what the name resolved to: a name allowed by the list or the mode may not
    /// point at a host-local address (`allow_local` names are what they are).
    pub fn decide_addrs(&self, rule: &'static str, addrs: &[IpAddr]) -> Decision {
        if addrs.is_empty() {
            return Decision::Deny("no address".into());
        }
        if rule != "allow_local" && addrs.iter().any(|a| is_local(*a)) {
            return Decision::Deny("host-local destination".into());
        }
        Decision::Allow(rule)
    }

    /// Both stages, with the addresses already known (tests, callers that resolved themselves).
    pub fn decide(&self, host: &str, port: u16, default_port: u16, addrs: &[IpAddr]) -> Decision {
        match self.decide_name(host, port, default_port) {
            Decision::Allow(rule) => self.decide_addrs(rule, addrs),
            deny => deny,
        }
    }
}

/// Serve forever on an already-bound listener. Returns only if the listener itself is gone.
pub fn serve(listener: UnixListener, rules: Rules, log: impl Fn(&str) + Send + Sync + 'static) -> Result<()> {
    let rules = Arc::new(rules);
    let log = Arc::new(log);
    let active = Arc::new(AtomicUsize::new(0));
    let mut failures = 0u32;
    loop {
        let conn = match listener.accept() {
            Ok((c, _)) => {
                failures = 0;
                c
            }
            Err(e) => {
                // out of fds, out of memory, a transient refusal: wait a moment and go on; the
                // listener is the session's only way out and must not die with one bad accept
                failures += 1;
                if failures <= 3 || failures % 100 == 0 {
                    log(&format!("accept error: {e} ({failures} in a row)"));
                }
                std::thread::sleep(Duration::from_millis(100.min(10 * failures as u64).max(10)));
                if failures > 10_000 {
                    bail!("accept keeps failing: {e}");
                }
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            log(&format!("deny (too many connections: {MAX_CONNECTIONS} open)"));
            let mut c = conn;
            let _ = respond(&mut c, "503 Service Unavailable", "sill: too many connections");
            continue;
        }
        let (rules, thread_log, counted) = (rules.clone(), log.clone(), active.clone());
        let spawned = std::thread::Builder::new().name("sill-proxy-conn".into()).spawn(move || {
            struct Done(Arc<AtomicUsize>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _done = Done(counted);
            if let Err(e) = handle(conn, &rules, &*thread_log) {
                thread_log(&format!("error: {e:#}"));
            }
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
            log("error: could not start a connection thread");
        }
    }
}

/// One line of the request head, at most `MAX_LINE` bytes; `None` at end of stream.
fn read_head_line(reader: &mut BufReader<UnixStream>) -> Result<Option<String>> {
    let mut line = String::new();
    let n = reader.by_ref().take(MAX_LINE as u64).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && n >= MAX_LINE {
        bail!("header line longer than {MAX_LINE} bytes");
    }
    Ok(Some(line))
}

fn handle(mut client: UnixStream, rules: &Rules, log: &dyn Fn(&str)) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let head = (|| -> Result<Option<(String, Vec<String>)>> {
        let Some(request_line) = read_head_line(&mut reader)? else { return Ok(None) };
        let mut headers = Vec::new();
        loop {
            let Some(line) = read_head_line(&mut reader)? else { break };
            if line == "\r\n" || line == "\n" {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                bail!("more than {MAX_HEADERS} header lines");
            }
            headers.push(line);
        }
        Ok(Some((request_line, headers)))
    })();
    let (request_line, headers) = match head {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(()),
        Err(e) => {
            respond(&mut client, "431 Request Header Fields Too Large", "sill: request head too large")?;
            return Err(e);
        }
    };
    let mut parts = request_line.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m.to_string(), t.to_string()),
        _ => bail!("bad request line {request_line:?}"),
    };

    let (host, port, default_port, origin_form) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(&target, CONNECT_PORT)?;
        (h, p, CONNECT_PORT, None)
    } else {
        // absolute-form: GET http://host[:port]/path
        let rest = target.strip_prefix("http://").context("only absolute-form http:// requests are proxied")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (h, p) = split_host_port(hostport, HTTP_PORT)?;
        (h, p, HTTP_PORT, Some(path.to_string()))
    };

    // the name first: nothing is resolved that the rules would not let through
    let rule = match rules.decide_name(&host, port, default_port) {
        Decision::Deny(why) => {
            log(&format!("deny {host}:{port} ({why})"));
            respond(&mut client, "403 Forbidden", &format!("sill: {why}"))?;
            return Ok(());
        }
        Decision::Allow(rule) => rule,
    };
    let addrs: Vec<IpAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.map(|s| s.ip()).collect(),
        Err(e) => {
            // the same answer as a refusal: resolvability is not for the jail to learn
            log(&format!("deny {host}:{port} (resolve: {e})"));
            respond(&mut client, "403 Forbidden", "sill: cannot resolve host")?;
            return Ok(());
        }
    };
    match rules.decide_addrs(rule, &addrs) {
        Decision::Deny(why) => {
            log(&format!("deny {host}:{port} ({why})"));
            respond(&mut client, "403 Forbidden", &format!("sill: {why}"))?;
            return Ok(());
        }
        Decision::Allow(rule) => log(&format!("allow {host}:{port} ({rule})")),
    }

    let mut upstream = TcpStream::connect_timeout(&std::net::SocketAddr::new(addrs[0], port), Duration::from_secs(20))
        .with_context(|| format!("connect {host}:{port}"))?;
    client.set_read_timeout(None)?;

    if let Some(path) = origin_form {
        // forward the request to the origin, one request per connection; hop-by-hop headers and
        // anything meant for a proxy stay here
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        for h in &headers {
            let lower = h.to_ascii_lowercase();
            if ["proxy-connection:", "proxy-authorization:", "connection:", "keep-alive:", "te:", "trailer:", "transfer-encoding:", "upgrade:"]
                .iter()
                .any(|k| lower.starts_with(k))
            {
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn names_the_shelf_may_show() {
        for ok in ["api.anthropic.com", "localhost", "a-b.example", "10.0.0.1", "::1", "x.y.z.example.org"] {
            assert!(valid_hostname(ok), "{ok}");
        }
        for bad in ["*", "*.example.com", "", "exa mple.com", "ex\u{202e}ample.com", "-a.example.com", "a..b", "a.example.com.", "a_b.example.com", "ünïcode.example"] {
            assert!(!valid_hostname(bad), "{bad:?}");
        }
        assert_eq!(wildcard_offer("api.example.com").as_deref(), Some("*.example.com"));
        assert_eq!(wildcard_offer("a.b.example.com").as_deref(), Some("*.b.example.com"));
        assert_eq!(wildcard_offer("example.com"), None, "two labels: no wildcard");
        assert_eq!(wildcard_offer("evil.co.uk"), None, "a public suffix is not a site");
        assert_eq!(wildcard_offer("tenant.github.io"), None);
        assert_eq!(wildcard_offer("*.example.com"), None);
        assert_eq!(wildcard_offer("10.0.0.1"), None);
    }

    #[test]
    fn patterns() {
        assert!(host_matches("*.anthropic.com", "api.anthropic.com"));
        assert!(!host_matches("*.anthropic.com", "anthropic.com"));
        assert!(!host_matches("*.anthropic.com", "evilanthropic.com"));
        assert!(host_matches("api.anthropic.com", "API.anthropic.com"));
        assert!(host_matches("*", "anything"));
        assert!(host_matches("localhost:8080", "localhost"));
    }

    #[test]
    fn ports_follow_the_pattern() {
        // bare: the request form's own port only
        assert!(pattern_matches("api.example.com", "api.example.com", 443, CONNECT_PORT));
        assert!(pattern_matches("api.example.com", "api.example.com", 80, HTTP_PORT));
        assert!(!pattern_matches("api.example.com", "api.example.com", 8443, CONNECT_PORT));
        assert!(!pattern_matches("api.example.com", "api.example.com", 22, CONNECT_PORT));
        // explicit port, any port
        assert!(pattern_matches("api.example.com:8443", "api.example.com", 8443, CONNECT_PORT));
        assert!(!pattern_matches("api.example.com:8443", "api.example.com", 443, CONNECT_PORT));
        assert!(pattern_matches("*.example.com:*", "x.example.com", 22, CONNECT_PORT));
        assert!(pattern_matches("[::1]:8080", "::1", 8080, CONNECT_PORT));
        assert!(!pattern_matches("[::1]:8080", "::1", 443, CONNECT_PORT));
        assert_eq!(split_pattern("::1"), ("::1", PortRule::Default));
        assert_eq!(split_pattern("localhost:*"), ("localhost", PortRule::Any));
    }

    #[test]
    fn names_are_decided_before_any_resolving() {
        let r = Rules { mode: NetMode::None, allow: vec![], allow_local: vec![] };
        assert!(matches!(r.decide_name("example.com", 443, CONNECT_PORT), Decision::Deny(_)));
        let r = Rules { mode: NetMode::Allow, allow: vec!["*.anthropic.com".into()], allow_local: vec!["localhost".into()] };
        assert_eq!(r.decide_name("api.anthropic.com", 443, CONNECT_PORT), Decision::Allow("allow-list"));
        assert!(matches!(r.decide_name("github.com", 443, CONNECT_PORT), Decision::Deny(_)));
        assert!(matches!(r.decide_name("api.anthropic.com", 8443, CONNECT_PORT), Decision::Deny(_)));
        assert_eq!(r.decide_name("localhost", 443, CONNECT_PORT), Decision::Allow("allow_local"));
        assert!(matches!(r.decide_name("localhost", 8080, CONNECT_PORT), Decision::Deny(_)));
        // a literal local address is refused without a lookup, in every mode
        let all = Rules { mode: NetMode::All, allow: vec![], allow_local: vec![] };
        assert!(matches!(all.decide_name("10.0.0.5", 443, CONNECT_PORT), Decision::Deny(_)));
        assert!(matches!(all.decide_name("127.0.0.1", 443, CONNECT_PORT), Decision::Deny(_)));
        assert_eq!(all.decide_name("example.com", 443, CONNECT_PORT), Decision::Allow("all"));
    }

    #[test]
    fn local_refused_even_when_all() {
        let r = Rules { mode: NetMode::All, allow: vec![], allow_local: vec![] };
        assert!(matches!(r.decide("localhost", 443, CONNECT_PORT, &[ip("127.0.0.1")]), Decision::Deny(_)));
        assert!(matches!(r.decide("10.0.0.5", 443, CONNECT_PORT, &[]), Decision::Deny(_)));
        assert!(matches!(r.decide("example.com", 443, CONNECT_PORT, &[ip("93.184.216.34")]), Decision::Allow(_)));
        assert!(matches!(r.decide("example.com", 443, CONNECT_PORT, &[]), Decision::Deny(_)));
        let r = Rules { mode: NetMode::Allow, allow: vec!["*.anthropic.com".into()], allow_local: vec!["localhost".into()] };
        assert!(matches!(r.decide("api.anthropic.com", 443, CONNECT_PORT, &[ip("1.2.3.4")]), Decision::Allow(_)));
        assert!(matches!(r.decide("github.com", 443, CONNECT_PORT, &[ip("1.2.3.4")]), Decision::Deny(_)));
        assert!(matches!(r.decide("localhost", 443, CONNECT_PORT, &[ip("127.0.0.1")]), Decision::Allow("allow_local")));
        // public name resolving to a private address: refused
        assert!(matches!(r.decide("api.anthropic.com", 443, CONNECT_PORT, &[ip("192.168.1.1")]), Decision::Deny(_)));
        assert!(matches!(r.decide("api.anthropic.com", 443, CONNECT_PORT, &[ip("1.2.3.4"), ip("fd00::1")]), Decision::Deny(_)));
    }

    #[test]
    fn reserved_ranges_are_local() {
        for a in ["0.0.0.0", "0.1.2.3", "127.0.0.1", "10.1.1.1", "172.16.0.1", "192.168.0.1", "169.254.1.1", "100.64.0.1", "192.0.0.8", "198.18.0.1", "198.19.255.255", "240.0.0.1", "255.255.255.255", "224.0.0.1"] {
            assert!(is_local(ip(a)), "{a}");
        }
        for a in ["::1", "::", "fe80::1", "fc00::1", "fd12::1", "ff02::1", "::ffff:10.0.0.1", "::ffff:127.0.0.1", "64:ff9b::7f00:1", "64:ff9b::a00:1", "2002:a00:1::1", "2002:c0a8:1::1"] {
            assert!(is_local(ip(a)), "{a}");
        }
        for a in ["8.8.8.8", "93.184.216.34", "198.17.0.1", "198.20.0.1", "100.63.0.1", "100.128.0.1", "2606:4700::1111", "64:ff9b::808:808", "2002:808:808::1", "::ffff:8.8.8.8"] {
            assert!(!is_local(ip(a)), "{a}");
        }
    }
}
