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
    pub fn pins(&self, id: &str) -> Result<Vec<PathBuf>> { super::remote::request(Request::Pins { session: id.into(), global: false }) }
    pub fn global_pins(&self) -> Result<Vec<PathBuf>> { super::remote::request(Request::GlobalPins) }
    pub fn entries_before(&self, id: &str, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        super::remote::request(Request::Entries { session: id.into(), before, limit })
    }
    pub fn memories(&self, id: &str) -> Result<Vec<(i64, String, String)>> { super::remote::request(Request::Memories { session: id.into() }) }
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<String>> { super::remote::request(Request::Search { text: text.into(), limit }) }
}
