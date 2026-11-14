//! The person's requests — send, stop, answer, a control, a pin — on a connection apart from the
//! session lanes, so none waits behind a snapshot or a stream. Calls block; a frontend runs each on
//! a thread of its own (the client takes concurrent calls on one connection).
//!
//! A mutation is never retried blindly (docs/protocol.md: "must not be blindly retried after
//! timeout/connection loss"). Its [`Outcome`] says what is known: done, refused by the daemon,
//! never sent, or uncertain because the connection went after the request may have arrived.
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::Value;
use sill_proto::Client;

use crate::{Connector, connection_lost, refusal_code};

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done(Value),
    /// The daemon answered with an error: the request did not take effect.
    Refused {
        code: Option<String>,
        message: String,
    },
    /// No connection could be made: the request never left.
    NotSent(String),
    /// The connection was lost, or the daemon did not answer, after the request went: it may have
    /// taken effect. Read the state before trying again.
    Uncertain(String),
}

impl Outcome {
    pub fn is_done(&self) -> bool {
        matches!(self, Outcome::Done(_))
    }

    /// What to tell the person, for anything but `Done`.
    pub fn problem(&self) -> Option<&str> {
        match self {
            Outcome::Done(_) => None,
            Outcome::Refused { message, .. }
            | Outcome::NotSent(message)
            | Outcome::Uncertain(message) => Some(message),
        }
    }
}

/// How a failed call ended: before sending, refused, or unknown.
pub fn classify(error: anyhow::Error) -> Outcome {
    let message = format!("{error:#}");
    if !connection_lost(&error) && error.downcast_ref::<sill_proto::rpc::Error>().is_some() {
        return Outcome::Refused {
            code: refusal_code(&error),
            message,
        };
    }
    if message.contains("exceeds max_message_bytes") {
        // refused locally, before anything was written
        return Outcome::Refused {
            code: Some("limit_exceeded".into()),
            message,
        };
    }
    Outcome::Uncertain(message)
}

pub struct Actions {
    connect: Connector,
    client: Mutex<Option<Arc<Client>>>,
}

impl Actions {
    pub fn new(connect: Connector) -> Arc<Self> {
        Arc::new(Self {
            connect,
            client: Mutex::new(None),
        })
    }

    /// The connection, made when there is none or the last one closed.
    fn client(&self) -> anyhow::Result<Arc<Client>> {
        let mut slot = self.client.lock().unwrap();
        if let Some(client) = slot.as_ref().filter(|c| c.is_open()) {
            return Ok(client.clone());
        }
        *slot = None;
        let client = Arc::new((self.connect)()?);
        *slot = Some(client.clone());
        Ok(client)
    }

    fn forget(&self, client: &Arc<Client>) {
        let mut slot = self.client.lock().unwrap();
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, client)) {
            *slot = None;
        }
    }

    /// One mutation, once.
    pub fn call(&self, method: &str, params: Value) -> Outcome {
        let client = match self.client() {
            Ok(client) => client,
            Err(e) => return Outcome::NotSent(format!("{e:#}")),
        };
        match client.call::<Value>(method, params) {
            Ok(value) => Outcome::Done(value),
            Err(e) => {
                if !client.is_open() {
                    self.forget(&client);
                }
                classify(e)
            }
        }
    }

    /// A read: safe to repeat, so a connection that turns out closed is replaced once.
    pub fn read<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let client = self.client()?;
        match client.call::<T>(method, params.clone()) {
            Err(e) if connection_lost(&e) => {
                self.forget(&client);
                self.client()?.call(method, params)
            }
            result => result,
        }
    }

    /// The sessions the daemon has, newest first.
    pub fn sessions(&self) -> anyhow::Result<Vec<sill_proto::sessions::Session>> {
        let client = self.client()?;
        match client.list_sessions() {
            Err(e) if connection_lost(&e) => {
                self.forget(&client);
                self.client()?.list_sessions()
            }
            result => result,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::fake::{connector, pair};
    use serde_json::json;

    #[test]
    fn outcomes_tell_refused_from_never_sent_from_uncertain() {
        let (client, mut peer) = pair();
        let actions = Actions::new(connector(vec![client]));
        let server = std::thread::spawn(move || {
            let first = peer.expect("_sill/sessions.submit");
            peer.refuse(&first, "invalid_state", "the session has ended");
            let second = peer.expect("_sill/sessions.cancel");
            peer.reply(&second, json!({"requested": true}));
            let _third = peer.expect("_sill/questions.answer");
            // the daemon goes away with the request in hand
        });
        let refused = actions.call("_sill/sessions.submit", json!({"text":"hi"}));
        assert_eq!(
            refused,
            Outcome::Refused {
                code: Some("invalid_state".into()),
                message: "_sill/sessions.submit: the session has ended".into()
            }
        );
        assert_eq!(
            actions.call("_sill/sessions.cancel", json!({})),
            Outcome::Done(json!({"requested": true}))
        );
        let lost = actions.call("_sill/questions.answer", json!({}));
        assert!(matches!(lost, Outcome::Uncertain(_)), "{lost:?}");
        server.join().unwrap();
        // the connection is gone and no new one can be made: the next request never leaves
        let unsent = actions.call("_sill/sessions.cancel", json!({}));
        assert!(matches!(unsent, Outcome::NotSent(_)), "{unsent:?}");
    }
}
