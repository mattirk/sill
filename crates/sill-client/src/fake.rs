//! A scripted daemon on the other end of a socket pair, for tests: read the client's requests,
//! answer, refuse, notify, or hang up.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sill_proto::Client;

pub struct Peer {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Peer {
    /// The next request; it must be `method`.
    pub fn expect(&mut self, method: &str) -> Value {
        let request = self
            .next()
            .unwrap_or_else(|| panic!("no request, expected {method}"));
        assert_eq!(request["method"], method, "{request}");
        request
    }

    /// The next request, or none when the client hung up.
    pub fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap()),
        }
    }

    pub fn reply(&mut self, request: &Value, result: impl serde::Serialize) {
        self.send(json!({"jsonrpc":"2.0", "id":request["id"], "result":result}));
    }

    pub fn refuse(&mut self, request: &Value, code: &str, message: &str) {
        self.send(json!({"jsonrpc":"2.0", "id":request["id"],
            "error":{"code":-32000, "message":message, "data":{"code":code}}}));
    }

    pub fn notify(&mut self, method: &str, params: impl serde::Serialize) {
        self.send(json!({"jsonrpc":"2.0", "method":method, "params":params}));
    }

    fn send(&mut self, value: Value) {
        let _ = writeln!(self.writer, "{value}");
    }

    /// The client closed its end: reading sees the end within the timeout.
    pub fn hung_up(&mut self) -> bool {
        self.next().is_none()
    }
}

/// A connected client and the fake daemon's end of it.
pub fn pair() -> (Client, Peer) {
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let reader = BufReader::new(peer.try_clone().unwrap());
    (
        Client::over(socket).unwrap(),
        Peer {
            reader,
            writer: peer,
        },
    )
}

/// A connector handing out one prepared client per connection; with none left it fails, as a
/// daemon that is down does.
pub fn connector(clients: Vec<Client>) -> crate::Connector {
    let clients = Arc::new(Mutex::new(
        clients.into_iter().map(Some).collect::<Vec<_>>(),
    ));
    Arc::new(move || {
        let mut all = clients.lock().unwrap();
        match all.iter_mut().find_map(Option::take) {
            Some(client) => Ok(client),
            None => anyhow::bail!("sill daemon at /fake: connection refused"),
        }
    })
}
