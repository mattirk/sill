//! Conversion at the main daemon's frontend boundary.
//!
//! The private relay owns its serialized types independently of the frontend contract. Keep
//! frontend adapters here so public protocol changes never reach the worker's transport.

use sill_core::wire;
use super::relay::Command;

/// A frontend's command, once the daemon took what is the daemon's (a prompt's transcript entry).
impl From<wire::Command> for Command {
    fn from(command: wire::Command) -> Self {
        match command {
            wire::Command::Prompt { text, attachments, entry: _ } => Command::Prompt { text, attachments: attachments.into_iter().map(Into::into).collect() },
            wire::Command::Authenticate(m) => Command::Authenticate(m),
            // the internal path keeps no keys: the agent refuses the login and asks again
            wire::Command::AuthenticateWithKey { method_id, .. } => Command::Authenticate(method_id),
            wire::Command::RetrySession => Command::RetrySession,
            wire::Command::SetMode(m) => Command::SetMode(m),
            wire::Command::SetConfig(o, v) => Command::SetConfig(o, v.into()),
            wire::Command::Cancel => Command::Cancel,
            wire::Command::Quit => Command::Quit,
        }
    }
}
