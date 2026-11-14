//! Local presentation conversions over public SFP only. Exact paths never pass through legacy JSON.
use anyhow::{Result, bail};
use serde_json::Value;
use sill_proto::{changes as api, sessions::HostPath as P, wire};
type Call<'a> = &'a dyn Fn(&str, Value) -> Result<Value>;
fn invoke<T: serde::de::DeserializeOwned>(
    call: Call<'_>,
    method: &str,
    input: impl serde::Serialize,
) -> Result<T> {
    Ok(serde_json::from_value(call(
        method,
        serde_json::to_value(input)?,
    )?)?)
}
pub fn list(call: Call<'_>) -> Result<wire::ChangesList> {
    let mut result = wire::ChangesList::default();
    let mut after = None;
    loop {
        let page: api::Page = invoke(
            call,
            "_sill/changes.list",
            api::List {
                after,
                limit: Some(128),
            },
        )?;
        for row in page.entries {
            match row {
                api::Entry::Live { repo, path, status } => {
                    let repo = repo.to_path()?;
                    let path = path.to_path()?.strip_prefix(&repo)?.to_path_buf();
                    let index = result
                        .live
                        .iter()
                        .position(|r| r.repo == repo)
                        .unwrap_or_else(|| {
                            result.live.push(wire::LiveRepo {
                                repo: repo.clone(),
                                status: vec![],
                            });
                            result.live.len() - 1
                        });
                    result.live[index].status.push((path, status));
                }
                api::Entry::Overlay {
                    session_id,
                    title,
                    ghost,
                    agent_state,
                    path,
                    kind,
                    file_type,
                    size,
                    conflict,
                    written,
                    repo,
                    hash,
                    protected,
                } => {
                    let kind = match kind.as_str() {
                        "added" => wire::Kind::Added,
                        "modified" => wire::Kind::Modified,
                        "deleted" => wire::Kind::Deleted,
                        "type_changed" => wire::Kind::TypeChanged,
                        "replaced" => wire::Kind::Replaced,
                        _ => bail!("unsupported change kind {kind}"),
                    };
                    let file_type = match file_type.as_str() {
                        "file" => wire::FileType::File,
                        "dir" => wire::FileType::Dir,
                        "symlink" => wire::FileType::Symlink,
                        "other" => wire::FileType::Other,
                        _ => bail!("unsupported file type {file_type}"),
                    };
                    let row = wire::ChangeRow {
                        change: wire::Change {
                            path: path.to_path()?,
                            upper: Default::default(),
                            kind,
                            file_type,
                            size: size.parse()?,
                            conflict,
                            ignored: false,
                        },
                        written: written.map(|v| v.parse()).transpose()?,
                        repo: repo.map(|p| p.to_path()).transpose()?.unwrap_or_default(),
                        hash,
                        protected,
                    };
                    let s = session(&mut result, session_id, title, ghost);
                    if agent_state {
                        s.agent_state.push(row);
                    } else {
                        s.changes.push(row);
                    }
                }
                api::Entry::Kept {
                    session_id,
                    title,
                    ghost,
                    keep_id,
                    timestamp,
                    path,
                    what,
                    before,
                    hash,
                } => {
                    session(&mut result, session_id, title, ghost)
                        .kept
                        .push(wire::Kept {
                            id: keep_id,
                            ts: timestamp.parse()?,
                            path: path.to_path()?,
                            what,
                            before,
                            hash,
                            undone_at: None,
                        });
                }
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    Ok(result)
}
fn session(
    out: &mut wire::ChangesList,
    id: String,
    title: String,
    ghost: bool,
) -> &mut wire::SessionChanges {
    let index = out
        .sessions
        .iter()
        .position(|s| s.id == id)
        .unwrap_or_else(|| {
            out.sessions.push(wire::SessionChanges {
                id,
                title,
                ghost,
                changes: vec![],
                agent_state: vec![],
                kept: vec![],
            });
            out.sessions.len() - 1
        });
    &mut out.sessions[index]
}
pub fn diff(call: Call<'_>, of: wire::DiffOf) -> Result<sill_proto::Text> {
    let target = match of {
        wire::DiffOf::Change { session, path } => api::Target::Change {
            session_id: session,
            path: P::from_path(&path),
        },
        wire::DiffOf::Live { repo, rel } => api::Target::Live {
            path: P::from_path(&repo.join(rel)),
            repo: P::from_path(&repo),
        },
        wire::DiffOf::Kept { session, id } => api::Target::Kept {
            session_id: session,
            keep_id: id,
        },
    };
    let mut offset = None;
    let mut fingerprint = None;
    let mut text = String::new();
    loop {
        let p: api::DiffPage = invoke(
            call,
            "_sill/changes.diff",
            api::Diff {
                target: target.clone(),
                offset,
                fingerprint,
            },
        )?;
        text.push_str(&p.text);
        offset = p.next_offset;
        fingerprint = Some(p.fingerprint);
        if offset.is_none() {
            break;
        }
    }
    Ok(sill_proto::Text { text })
}

pub fn keep(call: Call<'_>, a: sill_proto::ChangesKeep) -> Result<wire::Done> {
    anyhow::ensure!(
        a.items.len() <= 128,
        "select at most 128 changes per action"
    );
    if a.items.is_empty() {
        return Ok(wire::Done::default());
    }
    let items = a
        .items
        .into_iter()
        .map(|(p, h)| api::KeepItem {
            path: P::from_path(&p),
            expected_hash: h,
        })
        .collect();
    let d: api::Done = invoke(
        call,
        "_sill/changes.keep",
        api::Keep {
            session_id: a.session,
            items,
        },
    )?;
    d.try_into()
}
pub fn revert(call: Call<'_>, a: sill_proto::ChangesPaths) -> Result<wire::Done> {
    anyhow::ensure!(
        a.paths.len() <= 128,
        "select at most 128 changes per action"
    );
    if a.paths.is_empty() {
        return Ok(wire::Done::default());
    }
    let d: api::Done = invoke(
        call,
        "_sill/changes.revert",
        api::Revert {
            session_id: a.session,
            paths: a.paths.iter().map(|p| P::from_path(p)).collect(),
        },
    )?;
    d.try_into()
}
pub fn undo(call: Call<'_>, a: sill_proto::ChangesUndo) -> Result<wire::Done> {
    let ids = if a.ids.is_empty() {
        list(call)?
            .sessions
            .into_iter()
            .filter(|s| s.id == a.session)
            .flat_map(|s| s.kept.into_iter().map(|k| k.id))
            .collect()
    } else {
        a.ids
    };
    anyhow::ensure!(ids.len() <= 128, "select at most 128 keeps per undo action");
    if ids.is_empty() {
        return Ok(wire::Done::default());
    }
    let d: api::Done = invoke(
        call,
        "_sill/changes.undo",
        api::Undo {
            session_id: a.session,
            keep_ids: ids,
            force: a.force,
        },
    )?;
    d.try_into()
}
pub fn history(
    call: Call<'_>,
    f: sill_core::history::Filter,
) -> Result<Vec<sill_core::history::Event>> {
    let mut out = Vec::new();
    let mut before = None;
    let cap = if f.limit == 0 { 500 } else { f.limit };
    loop {
        let p: api::HistoryPage = invoke(
            call,
            "_sill/changes.history",
            api::History {
                before,
                since: f.since.map(|v| v.to_string()),
                path: f.path.as_ref().map(|p| P::from_path(p)),
                agent: f.agent.clone(),
                kind: f.kind.map(|k| k.id().into()),
                session_id: f.session.clone(),
                limit: Some((cap - out.len()).min(128) as u16),
            },
        )?;
        for e in p.events {
            out.push(sill_core::history::Event {
                id: e.event_id.parse()?,
                ts: e.timestamp.parse()?,
                kind: sill_core::history::Kind::parse(&e.kind)
                    .ok_or_else(|| anyhow::anyhow!("unsupported history kind {}", e.kind))?,
                session_id: e.session_id,
                session_title: e.session_title,
                agent: e.agent,
                ghost: e.ghost,
                path: e.path.map(|p| p.to_path()).transpose()?.unwrap_or_default(),
                what: e.what,
                hash_new: e.hash_new,
                hash_old: e.hash_old,
                count: e.count.parse()?,
                paths: e
                    .paths
                    .into_iter()
                    .map(|p| p.to_path())
                    .collect::<Result<_>>()?,
                kept_id: e.keep_id,
                head_start: e.head_start,
                head_end: e.head_end,
                commits: e.commits.map(|v| v.parse()).transpose()?,
                files: e.files.map(|v| v.parse()).transpose()?,
            });
        }
        before = p.next;
        if before.is_none() || out.len() >= cap {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_mutation_selection_is_refused_before_any_rpc() {
        let no_call = |_: &str, _: Value| -> Result<Value> {
            panic!("must not partially dispatch a selection")
        };
        let paths = vec![std::path::PathBuf::from("/fixture"); 129];
        assert!(
            keep(
                &no_call,
                sill_proto::ChangesKeep {
                    session: "12345678".into(),
                    items: paths.iter().map(|p| (p.clone(), None)).collect()
                }
            )
            .is_err()
        );
        assert!(
            revert(
                &no_call,
                sill_proto::ChangesPaths {
                    session: "12345678".into(),
                    paths
                }
            )
            .is_err()
        );
        assert!(
            undo(
                &no_call,
                sill_proto::ChangesUndo {
                    session: "12345678".into(),
                    ids: vec!["keep".into(); 129],
                    force: true
                }
            )
            .is_err()
        );
    }
    #[test]
    fn exact_paths_cross_public_mutation_without_legacy_json() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/fixture/\xff\nname".to_vec(),
        ));
        let call = |method: &str, value: Value| -> Result<Value> {
            assert_eq!(method, "_sill/changes.keep");
            let request: api::Keep = serde_json::from_value(value)?;
            assert_eq!(request.items[0].path.to_path()?, path);
            Ok(serde_json::to_value(api::Done {
                done: vec![api::Outcome {
                    path: P::from_path(&path),
                    message: "kept".into(),
                }],
                failed: vec![],
            })?)
        };
        let result = keep(
            &call,
            sill_proto::ChangesKeep {
                session: "12345678".into(),
                items: vec![(path.clone(), None)],
            },
        )
        .unwrap();
        assert_eq!(result.done[0].0, path);
    }
}
