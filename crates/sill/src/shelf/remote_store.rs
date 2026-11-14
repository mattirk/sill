//! Read facade for the daemon's store. No SQLite connection exists in the shelf.
use std::path::PathBuf;
use anyhow::Result;
use sill_core::frontend::Request;
pub use sill_core::store::{Entry, Session};

pub struct Store;
impl Store {
    pub fn open_default() -> Result<Self> {
        super::client::status()?;
        Ok(Self)
    }
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut after=None;let mut out=vec![];
        loop {
            let page:sill_proto::sessions::Page=super::client::call("_sill/sessions.list",sill_proto::sessions::List{after,limit:Some(128)})?;
            for s in page.sessions {
                out.push(Session{id:s.session_id,agent:s.agent,title:s.title,title_kind:s.title_kind,
                    created_at:s.created_at.parse()?,last_used:s.last_used.parse()?,ended_at:s.ended_at.map(|v|v.parse()).transpose()?,
                    acp_session_id:None,policy:s.policy,auto_allow:s.auto_allow,agent_settings:s.settings.into(),desktop:s.desktop,
                    summary:s.summary,summary_kind:s.summary_kind,memory_scope:s.memory_scope,follows:s.follows.unwrap_or_default(),
                    compacted:s.compacted.parse()?,ghost:s.ghost});
            }
            after=page.next;if after.is_none(){break;}
        }
        out.sort_by(|a,b|b.last_used.cmp(&a.last_used).then_with(||b.id.cmp(&a.id)));
        Ok(out)
    }
    pub fn pins(&self, id: &str) -> Result<Vec<PathBuf>> { self.pin_list(Some(id.into())) }
    pub fn global_pins(&self) -> Result<Vec<PathBuf>> { self.pin_list(None) }
    fn pin_list(&self, session_id: Option<String>) -> Result<Vec<PathBuf>> {
        let mut after = None;
        let mut out = Vec::new();
        loop {
            let page: sill_proto::pins::Page = super::client::call("_sill/pins.list", sill_proto::pins::List { session_id: session_id.clone(), after, limit: Some(128) })?;
            out.extend(page.pins.iter().map(|p| p.to_path()).collect::<Result<Vec<_>>>()?);
            after = page.next;
            if after.is_none() { return Ok(out); }
        }
    }
    pub fn entries_before(&self, id: &str, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        super::remote::request(Request::Entries { session: id.into(), before, limit })
    }
    pub fn memories(&self, id: &str) -> Result<Vec<(i64, String, String)>> {
        let mut after=None;let mut out=Vec::new();loop {
            let page:sill_proto::memory::Notes=super::client::call("_sill/memory.notes.list",sill_proto::memory::NotesList {session_id:id.into(),after,limit:Some(128)})?;
            for n in page.notes {out.push((n.timestamp.parse()?,n.kind,n.text));}after=page.next;if after.is_none(){return Ok(out);}
        }
    }
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<String>> {
        let mut after=None;let mut out=Vec::new();while out.len()<limit.min(500) {
            let page:sill_proto::memory::Hits=super::client::call("_sill/memory.search",sill_proto::memory::Search {query:text.into(),after,limit:Some((limit.min(500)-out.len()).min(128) as u16)})?;
            out.extend(page.hits.into_iter().map(|h|h.session_id));after=page.next;if after.is_none(){break;}
        }Ok(out)
    }
}
