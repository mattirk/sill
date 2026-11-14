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
    pub fn sessions(&self) -> Result<Vec<Session>> { super::remote::request(Request::Sessions) }
    pub fn pins(&self, id: &str) -> Result<Vec<PathBuf>> { super::remote::request(Request::Pins { session: id.into(), global: false }) }
    pub fn global_pins(&self) -> Result<Vec<PathBuf>> { super::remote::request(Request::GlobalPins) }
    pub fn entries_before(&self, id: &str, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        super::remote::request(Request::Entries { session: id.into(), before, limit })
    }
    pub fn memories(&self, id: &str) -> Result<Vec<(i64, String, String)>> { super::remote::request(Request::Memories { session: id.into() }) }
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<String>> { super::remote::request(Request::Search { text: text.into(), limit }) }
}
