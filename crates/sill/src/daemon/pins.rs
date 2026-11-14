//! Public pin methods. Exact host bytes are retained through storage and jail startup.
use super::*;
use sill_proto::{pins as api, sessions::HostPath};

fn pin_path(path: &HostPath) -> Result<PathBuf> { path.to_path().map_err(|e| Error::invalid_params(e).into()) }
fn list(store: &Store, a: api::List) -> Result<api::Page> {
    let n = a.limit.unwrap_or(64) as usize;
    if !(1..=128).contains(&n) { return Err(Error::invalid_params("limit must be 1–128").into()); }
    let after = a.after.as_ref().map(pin_path).transpose()?;
    let mut pins = if let Some(id) = a.session_id {
        if store.session_exact(&id)?.is_none() { return Err(Error::domain("not_found", "session not found").into()); }
        store.pins(&id)?
    } else { store.global_pins()? };
    use std::os::unix::ffi::OsStrExt;
    pins.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    pins.retain(|p| after.as_ref().is_none_or(|a| p.as_os_str().as_bytes() > a.as_os_str().as_bytes()));
    let more = pins.len() > n;
    pins.truncate(n);
    let next = if more { pins.last().map(|p| HostPath::from_path(p)) } else { None };
    Ok(api::Page { pins: pins.iter().map(|p| HostPath::from_path(p)).collect(), next })
}
impl Hub {
    pub(in crate::daemon) fn public_pins(self: &Arc<Self>, method: &str, params: Value) -> Option<Result<Value, Error>> {
        let result = || -> Result<Value> {
            match method {
                "_sill/pins.list" => self.with_store(|store| Ok(json!(list(store, args(params)?)?))),
                "_sill/pins.add" => {
                    let a: api::GlobalPin = args(params)?;
                    let path = pin_path(&a.path)?.canonicalize().map_err(|e| Error::domain(if e.kind() == std::io::ErrorKind::NotFound { "not_found" } else { "permission_denied" }, e.to_string()))?;
                    if !path.is_dir() && !path.is_file() { return Err(Error::invalid_params("pin must be a file or directory").into()); }
                    if path.starts_with(self.home.join(".local/state/sill")) || path == self.home {
                        return Err(Error::domain("permission_denied", "sill state and the home itself cannot be pinned").into());
                    }
                    self.with_store(|store| {
                        let held = store.global_pins()?;
                        if held.contains(&path) { return Ok(()); }
                        if held.iter().any(|p| path.starts_with(p) || p.starts_with(&path)) {
                            return Err(Error::domain("invalid_state", "global pins must not overlap").into());
                        }
                        if held.len() >= 128 { return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, Some(128), "at most 128 global pins").into()); }
                        store.global_pin(&path)
                    })?;
                    self.pins_changed(None);
                    Ok(json!(api::Pinned { path: HostPath::from_path(&path) }))
                }
                "_sill/pins.remove" => {
                    let a: api::GlobalPin = args(params)?;
                    let path = pin_path(&a.path)?;
                    self.with_store(|store| store.global_unpin(&path))?;
                    self.pins_changed(None);
                    Ok(json!({}))
                }
                "_sill/sessions.pin" | "_sill/sessions.unpin" => {
                    let a: api::SessionPin = args(params)?;
                    let path = pin_path(&a.path)?;
                    self.with_store(|store| {
                        let session = store.session_exact(&a.session_id)?.ok_or_else(|| Error::domain("not_found", "session not found"))?;
                        if method == "_sill/sessions.pin" {
                            if session.ghost { return Err(Error::domain("invalid_state", GHOST_NO_PINS).into()); }
                            let held = store.pins(&a.session_id)?;
                            if held.len() >= 128 && !held.contains(&path) { return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, Some(128), "at most 128 session pins").into()); }
                            store.pin(&a.session_id, &path)?;
                        } else { store.unpin(&a.session_id, &path)?; }
                        Ok(())
                    })?;
                    self.pins_changed(Some(&a.session_id));
                    Ok(json!({}))
                }
                _ => unreachable!(),
            }
        };
        if matches!(method, "_sill/pins.list" | "_sill/pins.add" | "_sill/pins.remove" | "_sill/sessions.pin" | "_sill/sessions.unpin") {
            Some(result().map_err(crate::daemon::sfp::failure))
        } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_path_pages_do_not_confuse_display_collisions_or_scope() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("sill-pin-page-{}", crate::daemon::sfp::id()));
        let store = Store::open(&dir.join("store.db")).unwrap();
        let session = store.create("test", "default").unwrap();
        let exact = PathBuf::from(std::ffi::OsString::from_vec(b"/pins/\xff".to_vec()));
        let display = PathBuf::from("/pins/\u{fffd}");
        store.global_pin(&display).unwrap();
        store.global_pin(&exact).unwrap();
        store.pin(&session.id, &exact).unwrap();
        let first = list(&store, api::List { limit: Some(1), ..Default::default() }).unwrap();
        assert_eq!(first.pins.len(), 1);
        let mut cursor = first.next.unwrap();
        cursor.display = "/irrelevant".into();
        let second = list(&store, api::List { after: Some(cursor), limit: Some(1), ..Default::default() }).unwrap();
        assert_eq!(second.pins.len(), 1);
        assert!(second.next.is_none());
        assert_ne!(first.pins[0].to_path().unwrap(), second.pins[0].to_path().unwrap());
        let scoped = list(&store, api::List { session_id: Some(session.id), ..Default::default() }).unwrap();
        assert_eq!(scoped.pins[0].to_path().unwrap(), exact);
        assert!(list(&store, api::List { limit: Some(129), ..Default::default() }).is_err());
        assert!(list(&store, api::List { session_id: Some("absent".into()), ..Default::default() }).is_err());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
