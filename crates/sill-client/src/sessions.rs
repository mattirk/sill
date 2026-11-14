//! Starting, reopening, ending and deleting sessions: the requests the `+` menu and a tab's close
//! make, built from public records only. Each is one mutation for
//! [`crate::actions::Actions`]; a new session's identity is the frontend's, so a retry after a
//! lost connection returns the same session instead of a second one (docs/protocol.md).
use serde_json::{Value, json};
use sill_proto::agent_admin::Definition;
use sill_proto::sessions::{Change, Create, End, Session, SessionKey, Update};

/// One request: the method and its parameters.
pub type Request = (&'static str, Value);

/// A new session of `agent` with the default policy. `remember_pins` (Settings → Remember pins)
/// asks the daemon to start it with the latest session's pins; a ghost never takes them.
pub fn create(agent: &str, ghost: bool, remember_pins: bool) -> Request {
    let create = Create {
        create_id: crate::request_id("create"),
        agent: agent.into(),
        policy: "default".into(),
        remember_pins: remember_pins && !ghost,
        ghost,
        ..Create::default()
    };
    ("_sill/sessions.create", json!(create))
}

/// *New here*: a new session with an ended one's agent, policy, pins and memory scope,
/// told that one's summary first.
pub fn new_here(from: &Session) -> Request {
    let create = Create {
        create_id: crate::request_id("create"),
        agent: from.agent.clone(),
        policy: from.policy.clone(),
        pins: from.pins.clone(),
        memory_scope: from.memory_scope.clone(),
        follows: from.session_id.clone(),
        remember_pins: false,
        ghost: false,
        workspaces: None,
    };
    ("_sill/sessions.create", json!(create))
}

/// An ended session open again; attaching then starts it.
pub fn reopen(session_id: &str) -> Request {
    let update = Update {
        session_id: session_id.into(),
        change: Change::Reopen,
    };
    ("_sill/sessions.update", json!(update))
}

/// End a session as closing its tab does: the summary, or a ghost's deletion.
pub fn end(session_id: &str) -> Request {
    let end = End {
        session_id: session_id.into(),
        reason: "close".into(),
    };
    ("_sill/sessions.end", json!(end))
}

/// Delete a session and its recorded changes.
pub fn delete(session_id: &str) -> Request {
    let key = SessionKey {
        session_id: session_id.into(),
    };
    ("_sill/sessions.delete", json!(key))
}

/// The session a `sessions.create` answered with.
pub fn created(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .and_then(|id| id.as_str())
        .map(str::to_owned)
}

/// What the Reopen list offers: sessions not open as tabs, newest first, without ended ghosts
/// (they are gone).
pub fn reopenable<'a>(all: &'a [Session], open: &[String]) -> Vec<&'a Session> {
    all.iter()
        .filter(|s| !open.contains(&s.session_id))
        .filter(|s| !(s.ghost && s.ended_at.is_some()))
        .collect()
}

/// Label outside session tabs: prefer the descriptive title once one has been supplied.
/// Until then, use the current session name rather than its old generated placeholder.
pub fn display_title(session: &Session) -> &str {
    if !session.title_kind.is_empty() && !session.title.trim().is_empty() {
        &session.title
    } else {
        session.name.as_deref().unwrap_or(&session.title)
    }
}

/// `session` matches a typed filter: every word in its title or summary, ignoring case.
pub fn matches(session: &Session, filter: &str) -> bool {
    let haystack = format!("{} {} {}", session.name.as_deref().unwrap_or(""), session.title, session.summary).to_lowercase();
    filter
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

/// The agents a new session can start with: enabled, and their command present on the daemon's
/// host (as the `+` menu shows them).
pub fn startable(client: &sill_proto::Client) -> anyhow::Result<Vec<Definition>> {
    use sill_proto::agent_admin::{AgentKey, Available, List, Page};
    let mut agents = vec![];
    let mut after = None;
    loop {
        let page: Page = client.call(
            "_sill/agents.list",
            List {
                after,
                limit: Some(64),
            },
        )?;
        agents.extend(page.agents.into_iter().filter(|a| a.enabled));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let mut startable = vec![];
    for agent in agents {
        let available: Available = client.call(
            "_sill/agents.available",
            AgentKey {
                agent_id: agent.id.clone(),
            },
        )?;
        if available.path.is_some() {
            startable.push(agent);
        }
    }
    Ok(startable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, ended: bool, ghost: bool) -> Session {
        serde_json::from_value(json!({
            "session_id": id, "agent": "claude", "title": format!("title {id}"), "title_kind": "",
            "created_at": "1", "last_used": "1", "ended_at": ended.then_some("2"),
            "policy": "strict", "auto_allow": false, "memory_scope": "here",
            "summary": "fixed the send arrow", "summary_kind": "agent", "ghost": ghost,
            "running": false, "pins": [{"bytes_base64": "L3A=", "display": "/p"}],
            "settings": {}, "follows": null, "compacted": "0"
        }))
        .unwrap()
    }

    #[test]
    fn labels_prefer_summary_titles_and_fall_back_to_current_names() {
        let mut s = session("old", false, false);
        s.name = Some("human name".into());
        assert_eq!(display_title(&s), "human name");
        s.title = "A descriptive summary title".into();
        s.title_kind = "agent".into();
        assert_eq!(display_title(&s), "A descriptive summary title");
        s.name = Some("renamed again".into());
        assert_eq!(display_title(&s), "A descriptive summary title");
        s.title.clear();
        assert_eq!(display_title(&s), "renamed again");
        s.name = None;
        s.title = "A title without a name".into();
        s.title_kind.clear();
        assert_eq!(display_title(&s), "A title without a name");
    }

    #[test]
    fn requests_have_the_shapes_the_daemon_accepts() {
        let (method, params) = create("codex", true, true);
        assert_eq!(method, "_sill/sessions.create");
        let create: Create = serde_json::from_value(params.clone()).unwrap();
        assert!(
            create.ghost && !create.remember_pins,
            "a ghost never takes remembered pins"
        );
        assert!(create.create_id.starts_with("gui-create-"));
        let from = session("old", true, false);
        let (_, params) = new_here(&from);
        let create: Create = serde_json::from_value(params).unwrap();
        assert_eq!(
            (
                create.follows.as_str(),
                create.policy.as_str(),
                create.pins.len()
            ),
            ("old", "strict", 1)
        );
        assert_eq!(
            reopen("s").1,
            json!({"session_id":"s", "change":{"type":"reopen"}})
        );
        assert_eq!(end("s").1, json!({"session_id":"s", "reason":"close"}));
        assert_eq!(delete("s").1, json!({"session_id":"s"}));
        assert_eq!(created(&json!({"session_id":"n"})).as_deref(), Some("n"));
    }

    #[test]
    fn reopen_lists_closed_sessions_but_not_ended_ghosts_and_filters_by_words() {
        let all = vec![
            session("a", true, false),
            session("b", true, true),
            session("c", false, true),
            session("d", false, false),
        ];
        let ids: Vec<&str> = reopenable(&all, &["d".into()])
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "c"]);
        assert!(matches(&all[0], "SEND title"));
        assert!(!matches(&all[0], "send nothing"));
    }
}
