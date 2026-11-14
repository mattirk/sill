//! An agent's login: the session
//! says it needs one (`SessionState::authentication`) and lists the agent's methods; each method
//! goes one of three ways, decided by the agent's definition on the daemon:
//!
//! - **on the host**: a browser flow with a localhost callback cannot finish inside the jail
//!   (Codex's ChatGPT, Gemini's Google login), so the daemon runs it on its host
//!   (`_sill/agents.login`) and the session starts again with the new login bound in;
//! - **in the jail's terminal**: the agent's own login command (Claude: `--cli auth login …`) in
//!   a terminal of the session's jail; when it exits the session is told to go on
//!   (`retry_session`);
//! - **through the agent**: `authenticate`, with the key the person typed for a method that takes
//!   one (the daemon keeps it outside the jail).
//!
//! No widget here: a frontend shows the card and calls these.
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use sill_proto::Client;
use sill_proto::agent_admin::{AgentKey, Definition};
use sill_proto::host::{AgentLogin, JobStream, OperationState, Progress};
use sill_proto::sessions::SessionKey;
use sill_proto::sfp::AuthMethod;

/// The jail terminal a login runs in, apart from the person's own shell.
pub const TERMINAL_LABEL: &str = "login";

/// How a login method runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// On the daemon's host: [`host_login`], then the session starts again.
    Host,
    /// In a terminal of the session's jail: this command and environment.
    Terminal {
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// The agent's own `authenticate`, no key.
    Agent,
}

/// Which way `method` goes for `agent`: the agent's host-login methods
/// and every protocol-level method of an agent installed from the registry (no home paths, no
/// curated method list) go to the host; a method with a terminal command runs it in the jail;
/// the rest is the agent's own.
pub fn route(agent: &Definition, method: &AuthMethod) -> Route {
    let unknown_agent = agent.home_paths.is_empty();
    if agent.host_login.contains(&method.method_id) || (unknown_agent && method.terminal.is_none())
    {
        return Route::Host;
    }
    match &method.terminal {
        Some(terminal) => {
            let mut argv = agent.command.clone();
            argv.extend(terminal.args.iter().cloned());
            let mut env = agent.env.clone();
            env.extend(terminal.env.clone());
            Route::Terminal { argv, env }
        }
        None => Route::Agent,
    }
}

/// The definition of the agent `session_id` runs, and whether its jail was started again first:
/// a jail keeps the policy it was built with, and a login under an older one can be refused its
/// own endpoints (Claude's `platform.claude.com`, allowed after the jail started, 2026-11-05).
pub fn prepare(client: &Client, session_id: &str) -> Result<(Definition, bool)> {
    /// What is read of the session: its agent, and whether its policy changed under its jail.
    #[derive(serde::Deserialize)]
    struct Session {
        agent: String,
        #[serde(default)]
        running: bool,
        #[serde(default)]
        policy_changed: bool,
    }
    let session: Session = client.call(
        "_sill/sessions.get",
        SessionKey {
            session_id: session_id.into(),
        },
    )?;
    let agent: Definition = client
        .call(
            "_sill/agents.get",
            AgentKey {
                agent_id: session.agent.clone(),
            },
        )
        .with_context(|| format!("reading agent {}", session.agent))?;
    let restart_first = session.running && session.policy_changed;
    if restart_first {
        restart(client, session_id)?;
    }
    Ok((agent, restart_first))
}

/// Run `method` of `agent_id` on the daemon's host, telling `progress` each line it reports;
/// `Ok` once the login completed. A lost stream leaves the outcome unknown: an error that says so.
pub fn host_login(
    client: &Client,
    agent_id: &str,
    method_id: &str,
    mut progress: impl FnMut(String),
) -> Result<()> {
    use std::io::BufRead;
    let job: JobStream = client.call(
        "_sill/agents.login",
        AgentLogin {
            operation_id: Some(crate::request_id("login")),
            agent_id: agent_id.into(),
            method_id: method_id.into(),
        },
    )?;
    let Some(token) = job.token else {
        // a job already known by this identity: its receipt, without its output
        return match job.operation.state {
            OperationState::Completed => Ok(()),
            state => {
                bail!("a login of {agent_id} is already {state:?}; check it before trying again")
            }
        };
    };
    let stream = sill_proto::transport::open_stream(&token)?;
    struct Close(std::sync::Arc<dyn Fn() + Send + Sync>);
    impl Drop for Close {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    let _close = Close(stream.interrupt.clone());
    for line in std::io::BufReader::new(stream.reader).lines() {
        match serde_json::from_str::<Progress>(&line?)? {
            Progress::Completed { .. } => return Ok(()),
            Progress::Failed { message, .. } => bail!("{message}"),
            Progress::Status { text } | Progress::Stderr { text } | Progress::Output { text } => {
                progress(text)
            }
        }
    }
    bail!(
        "the login's connection closed before it finished; check whether it worked before trying again"
    )
}

/// Start the session's jail again, with the login the host wrote bound in.
pub fn restart(client: &Client, session_id: &str) -> Result<()> {
    let key = SessionKey {
        session_id: session_id.into(),
    };
    let _: serde_json::Value = client.call("_sill/sessions.stop", &key)?;
    let _: serde_json::Value = client.call("_sill/sessions.start", &key)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(host_login: &[&str], home_paths: &[&str]) -> Definition {
        serde_json::from_value(json!({
            "id": "a", "name": "A", "command": ["agent", "--acp"], "env": {"A": "1"},
            "host_login": host_login, "home_paths": home_paths,
        }))
        .unwrap()
    }

    fn method(id: &str, terminal: Option<&[&str]>) -> AuthMethod {
        serde_json::from_value(json!({
            "method_id": id, "title": id, "description": null,
            "terminal": terminal.map(|args| json!({"args": args, "env": {"B": "2"}})),
        }))
        .unwrap()
    }

    #[test]
    fn methods_route_as_the_frontend_routes_them() {
        let curated = agent(&["chatgpt"], &[".codex"]);
        assert_eq!(route(&curated, &method("chatgpt", None)), Route::Host);
        assert_eq!(route(&curated, &method("api-key", None)), Route::Agent);
        assert_eq!(
            route(
                &curated,
                &method("claude-ai-login", Some(&["--cli", "auth", "login"]))
            ),
            Route::Terminal {
                argv: ["agent", "--acp", "--cli", "auth", "login"]
                    .map(String::from)
                    .into(),
                env: [("A", "1"), ("B", "2")]
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .into(),
            }
        );
        // an agent from the registry: its browser flows cannot finish in the jail
        let unknown = agent(&[], &[]);
        assert_eq!(route(&unknown, &method("oauth", None)), Route::Host);
    }
}
