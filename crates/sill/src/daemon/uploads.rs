//! Immutable attachment objects. Canonical bytes stay in the daemon database; jail-visible
//! copies are materialized from them, so a jail write cannot change retry identity.
use std::path::Path;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use sill_core::{acp, store::Store};
use sill_proto::{rpc::Error, sfp::{Upload, UploadCreate, UploadCreated, UploadGet}};

pub const MAX_UPLOAD_BYTES: usize = sill_proto::initialize::MAX_INLINE_BYTES;
/// Images the agent produced: stored under this reserved prefix with a budget of their own, so
/// agent output can never fill the space a person's uploads use (and the reverse).
pub const AGENT_PREFIX: &str = "sill-agent-";
pub const AGENT_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const AGENT_MEDIA_OBJECTS: i64 = 4096;
const AGENT_MEDIA_BYTES: i64 = 256 * 1024 * 1024;
/// The image types models accept as image blocks; other images (SVG, TIFF, HEIC…) go as files.
const IMAGE_BLOCK_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
fn domain(code: &str, message: &str) -> anyhow::Error { Error::domain(code, message).into() }
fn initialize(store: &Store) -> Result<()> {
    store.conn().execute_batch("CREATE TABLE IF NOT EXISTS sfp_uploads (
        session_id TEXT NOT NULL, upload_id TEXT NOT NULL, name TEXT NOT NULL,
        mime TEXT NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY(session_id, upload_id));
        CREATE TRIGGER IF NOT EXISTS sfp_uploads_delete AFTER DELETE ON sessions BEGIN
        DELETE FROM sfp_uploads WHERE session_id=OLD.id; END;")?;
    Ok(())
}
fn session(store: &Store, id: &str) -> Result<sill_core::store::Session> {
    store.sessions()?.into_iter().find(|s| s.id == id).ok_or_else(|| domain("not_found", "session not found"))
}

pub(super) fn create(store: &Store, request: UploadCreate) -> Result<UploadCreated> {
    if request.upload_id.starts_with("sill-inline-") || request.upload_id.starts_with(AGENT_PREFIX) { return Err(Error::invalid_params("sill-inline- and sill-agent- upload IDs are reserved").into()); }
    create_owned(store, request)
}
fn create_owned(store: &Store, request: UploadCreate) -> Result<UploadCreated> {
    session(store, &request.session_id)?;
    if request.upload_id.is_empty() || request.upload_id.len() > 128 || request.upload_id.chars().any(char::is_control) {
        return Err(Error::invalid_params("upload_id must contain 1–128 bytes without control characters").into());
    }
    if request.name.is_empty() || request.name.len() > 255 || request.name.chars().any(char::is_control)
        || Path::new(&request.name).file_name().and_then(|n|n.to_str()) != Some(request.name.as_str())
        || matches!(request.name.as_str(), "." | "..") {
        return Err(Error::invalid_params("invalid attachment name").into());
    }
    if request.mime.is_empty() || request.mime.len() > 256 || request.mime.chars().any(char::is_control) {
        return Err(Error::invalid_params("invalid attachment MIME type").into());
    }
    let bytes = decode(&request.data_base64)?;
    initialize(store)?;
    let old: Option<(String,String,Vec<u8>)> = store.conn().query_row(
        "SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![request.session_id, request.upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((name,mime,old_bytes)) = old {
        if name != request.name || mime != request.mime || bytes != old_bytes { return Err(domain("id_conflict", "upload ID already identifies different input")); }
    } else {
        let (count,total):(i64,i64)=store.conn().query_row("SELECT count(*),coalesce(sum(length(bytes)),0) FROM sfp_uploads WHERE session_id=?1 AND upload_id NOT LIKE 'sill-agent-%'",[&request.session_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if count>=4096 || total.saturating_add(bytes.len() as i64)>256*1024*1024 {return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, None, "session uploads exceed 4096 objects or 256 MiB")));}
        store.conn().execute("INSERT INTO sfp_uploads(session_id,upload_id,name,mime,bytes) VALUES (?1,?2,?3,?4,?5)",
            params![request.session_id,request.upload_id,request.name,request.mime,bytes])?;
    }
    Ok(UploadCreated { upload_id:request.upload_id })
}

pub(super) fn get(store: &Store, request: UploadGet) -> Result<Upload> {
    session(store, &request.session_id)?;
    initialize(store)?;
    let object: Option<(String,String,Vec<u8>)> = store.conn().query_row("SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![request.session_id,request.upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (name,mime,bytes)=object.ok_or_else(||domain("not_found","upload not found in this session"))?;
    Ok(Upload { upload_id: request.upload_id, name, mime, data_base64: acp::base64(&bytes) })
}

/// An inline attachment becomes an upload named by its submission and position, so the journal
/// references it and a retry of the same submission finds the same object.
pub(super) fn from_inline(store: &Store, session_id: &str, submission_id: &str, index: usize, name: &str, mime: &str, data_base64: &str) -> Result<(String, String)> {
    let upload_id = format!("sill-inline-{}-{index}", sill_core::sfp::create_fingerprint(submission_id));
    // inline names are labels ("screenshot-1.png (1180×518)"); an upload's is a file name
    let name = truncate(name.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect(), 255);
    let name = if name.is_empty() || name == "." || name == ".." { "image".to_string() } else { name };
    create_owned(store, UploadCreate { session_id: session_id.into(), upload_id: upload_id.clone(), name: name.clone(), mime: mime.into(), data_base64: data_base64.into() })?;
    Ok((upload_id, name))
}

/// Keep agent images (content-addressed, so a repeat is free). Returns the IDs refused because
/// the session's agent-media budget is full; their blocks then say `omitted: capacity`.
pub(super) fn keep_agent_media(store: &Store, session_id: &str, media: Vec<super::sfp_adapter::AgentMedia>) -> Result<std::collections::BTreeSet<String>> {
    let mut refused = std::collections::BTreeSet::new();
    if media.is_empty() { return Ok(refused); }
    initialize(store)?;
    for m in media {
        let exists: bool = store.conn().query_row("SELECT count(*) FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2", params![session_id, m.upload_id], |r| r.get::<_, i64>(0))? > 0;
        if exists { continue; }
        let (count, total): (i64, i64) = store.conn().query_row("SELECT count(*),coalesce(sum(length(bytes)),0) FROM sfp_uploads WHERE session_id=?1 AND upload_id LIKE 'sill-agent-%'", [session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if count >= AGENT_MEDIA_OBJECTS || total.saturating_add(m.bytes.len() as i64) > AGENT_MEDIA_BYTES { refused.insert(m.upload_id); continue; }
        let ext = match m.mime.as_str() { "image/png" => ".png", "image/jpeg" => ".jpg", "image/gif" => ".gif", "image/webp" => ".webp", "image/svg+xml" => ".svg", _ => "" };
        store.conn().execute("INSERT INTO sfp_uploads(session_id,upload_id,name,mime,bytes) VALUES (?1,?2,?3,?4,?5)",
            params![session_id, m.upload_id, format!("agent-image{ext}"), m.mime, m.bytes])?;
    }
    Ok(refused)
}

fn image_block(mime: &str, images: bool) -> bool { images && IMAGE_BLOCK_MIMES.contains(&mime) }

/// At most `max` bytes, cut at a character boundary (`create` limits bytes, not characters).
fn truncate(mut s: String, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) { end -= 1; }
    s.truncate(end);
    s
}

/// Verify the reference without filesystem writes; return image bytes for the submission budget.
pub(super) fn validate(store: &Store, session_id: &str, upload_id: &str, name: &str, mime: &str) -> Result<usize> {
    session(store,session_id)?;
    initialize(store)?;
    let metadata: Option<(String,String,i64)> = store.conn().query_row("SELECT name,mime,length(bytes) FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![session_id,upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let metadata=metadata.ok_or_else(||domain("not_found","upload not found in this session"))?;
    if (metadata.0.as_str(),metadata.1.as_str()) != (name,mime) { return Err(Error::invalid_params("attachment metadata differs from immutable upload").into()); }
    Ok(if mime.starts_with("image/") { usize::try_from(metadata.2)? } else { 0 })
}

/// Materialize a new private copy for this dispatch. Do not use jail-modifiable bytes as the
/// canonical object. The prepared command persists this chosen path for same-attempt recovery.
/// An image the agent can take (`images`: it said so in `initialize`) goes as an image block, as it
/// did when it was inline; anything else is a staged file.
pub(super) fn resolve(store: &Store, session_id: &str, upload_id: &str, images: bool) -> Result<acp::Attachment> {
    let row=session(store,session_id)?;
    initialize(store)?;
    let object: Option<(String,String,Vec<u8>)> = store.conn().query_row("SELECT name,mime,bytes FROM sfp_uploads WHERE session_id=?1 AND upload_id=?2",
        params![session_id,upload_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (name,mime,bytes)=object.ok_or_else(||domain("not_found","upload not found in this session"))?;
    if image_block(&mime, images) {
        return Ok(acp::Attachment{name,mime,kind:acp::AttachmentKind::Image{data_base64:acp::base64(&bytes)}});
    }
    let home=std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let policy=sill_core::session::policy_of(&row,&home)?;
    let state=sill_core::session::state_dir(rustix::process::getuid().as_raw(),session_id)?;
    let (host,jail)=sill_core::session::uploads_dir(&policy,&state,&home).ok_or_else(||domain("unsupported","policy has no private uploads directory"))?;
    let file=super::sfp::id();
    let path=host.join(&file);
    sill_core::safe_fs::write_file_beneath(&state,path.strip_prefix(&state)?,&bytes,0o644)?;
    Ok(acp::Attachment{name,mime,kind:acp::AttachmentKind::File{jail_path:jail.join(file)}})
}

pub(super) fn decode(text: &str) -> Result<Vec<u8>> { decode_limited(text, MAX_UPLOAD_BYTES) }
pub(super) fn decode_limited(text: &str, max: usize) -> Result<Vec<u8>> {
    let too_large = || anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(max as u64), format!("upload exceeds the {max}-byte decoded limit")));
    if text.len() > max.div_ceil(3)*4 { return Err(too_large()); }
    if text.len()%4 != 0 { return Err(Error::invalid_params("base64 must use canonical padding").into()); }
    let value=|b:u8|->Result<u8> { match b { b'A'..=b'Z'=>Ok(b-b'A'), b'a'..=b'z'=>Ok(b-b'a'+26), b'0'..=b'9'=>Ok(b-b'0'+52), b'+'=>Ok(62), b'/'=>Ok(63), _=>Err(Error::invalid_params("invalid base64").into()) } };
    let mut out=Vec::with_capacity(text.len()/4*3);
    let groups=text.as_bytes().chunks_exact(4);
    let count=groups.len();
    for (i,c) in groups.enumerate() {
        let a=value(c[0])?; let b=value(c[1])?;
        out.push((a<<2)|(b>>4));
        if c[2]==b'=' {
            if c[3]!=b'=' || i+1!=count || b&15!=0 { return Err(Error::invalid_params("invalid base64 padding").into()); }
        } else {
            let d=value(c[2])?; out.push((b<<4)|(d>>2));
            if c[3]==b'=' {
                if i+1!=count || d&3!=0 { return Err(Error::invalid_params("invalid base64 padding").into()); }
            } else { out.push((d<<6)|value(c[3])?); }
        }
    }
    if out.len()>max { return Err(too_large()); }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_images_the_agent_and_model_take_become_image_blocks() {
        assert!(image_block("image/png", true) && image_block("image/webp", true));
        assert!(!image_block("image/svg+xml", true) && !image_block("image/heic", true), "staged as files");
        assert!(!image_block("image/png", false), "an agent without image support gets a file");
    }
    #[test]
    fn truncation_counts_bytes_at_character_boundaries() {
        assert_eq!(truncate("ää".into(), 3), "ä");
        assert_eq!(truncate("abc".into(), 128), "abc");
    }
    #[test]
    fn upload_retry_is_immutable_scoped_and_removed_with_session() {
        let root=std::env::temp_dir().join(format!("sill-upload-test-{}-{}",std::process::id(),super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();
        let session=store.create_with("test","default",false).unwrap();
        let mut request=UploadCreate{session_id:session.id.clone(),upload_id:"upload".into(),name:"notes.txt".into(),mime:"text/plain".into(),data_base64:acp::base64(b"hello")};
        assert_eq!(create(&store,request.clone()).unwrap(),create(&store,request.clone()).unwrap());
        validate(&store,&session.id,"upload","notes.txt","text/plain").unwrap();
        assert!(validate(&store,&session.id,"upload","other.txt","text/plain").is_err());
        request.data_base64=acp::base64(b"changed");
        assert_eq!(create(&store,request.clone()).unwrap_err().downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"],"id_conflict");
        request.session_id="missing".into(); assert!(create(&store,request).is_err());
        store.delete(&session.id).unwrap();
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_uploads",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        drop(store); std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn inline_ids_do_not_collide_on_long_submission_prefixes_or_client_uploads() {
        let root=std::env::temp_dir().join(format!("sill-inline-test-{}",super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();let session=store.create("test","default").unwrap();
        let prefix="x".repeat(127);
        let a=from_inline(&store,&session.id,&format!("{prefix}a"),0,"image.png","image/png","YQ==").unwrap();
        let b=from_inline(&store,&session.id,&format!("{prefix}b"),0,"image.png","image/png","Yg==").unwrap();
        assert_ne!(a.0,b.0);assert!(a.0.len()<=128);
        assert!(create(&store,UploadCreate{session_id:session.id,upload_id:a.0,name:"image.png".into(),mime:"image/png".into(),data_base64:"YQ==".into()}).is_err());
        drop(store);std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inline_retry_preserves_identity_and_rejects_changed_bytes() {
        let root=std::env::temp_dir().join(format!("sill-inline-retry-test-{}",super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();let session=store.create("test","default").unwrap();
        let original=from_inline(&store,&session.id,"submission",0,"image.png","image/png","YQ==").unwrap();
        let journal=sill_core::sfp::Journal::new(&store).unwrap();journal.ensure(&session.id,"g").unwrap();
        let input=sill_proto::sfp::Submit{session_id:session.id.clone(),submission_id:"submission".into(),text:"work".into(),attachments:vec![sill_proto::sfp::Attachment::Uploaded{upload_id:original.0.clone(),name:original.1.clone(),mime:"image/png".into()}],purpose:None};
        journal.accept(&input,"queued").unwrap();
        assert_eq!(from_inline(&store,&session.id,"submission",0,"image.png","image/png","YQ==").unwrap(),original);
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_uploads",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        let error=from_inline(&store,&session.id,"submission",0,"image.png","image/png","Yg==").unwrap_err();
        assert_eq!(error.downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"],"id_conflict");
        drop(store);std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_upload_quota_allows_identical_retries_and_rejects_new_objects_atomically() {
        let root=std::env::temp_dir().join(format!("sill-upload-quota-test-{}",super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();let session=store.create("test","default").unwrap();
        let mut request=UploadCreate{session_id:session.id.clone(),upload_id:"existing".into(),name:"notes.txt".into(),mime:"text/plain".into(),data_base64:"YQ==".into()};
        let accepted=create(&store,request.clone()).unwrap();
        store.conn().execute("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<4095) INSERT INTO sfp_uploads SELECT ?1,'filled-'||n,'empty','text/plain',x'' FROM ids",[&session.id]).unwrap();
        assert_eq!(create(&store,request.clone()).unwrap(),accepted);
        request.upload_id="new".into();
        let error=create(&store,request).unwrap_err();
        assert_eq!(error.downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"],"limit_exceeded");
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_uploads",[],|r|r.get::<_,i64>(0)).unwrap(),4096);
        drop(store);std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_images_have_their_own_budget_and_reserved_ids() {
        use super::super::sfp_adapter::AgentMedia;
        let root=std::env::temp_dir().join(format!("sill-agent-media-test-{}",super::super::sfp::id()));
        let store=Store::open(&root.join("store.db")).unwrap();let session=store.create("test","default").unwrap();
        let media=|id:&str|AgentMedia{upload_id:format!("{AGENT_PREFIX}{id}"),mime:"image/png".into(),bytes:b"png".to_vec()};
        assert!(keep_agent_media(&store,&session.id,vec![media("a"),media("a")]).unwrap().is_empty());
        let kept=get(&store,UploadGet{session_id:session.id.clone(),upload_id:format!("{AGENT_PREFIX}a")}).unwrap();
        assert_eq!((kept.name.as_str(),kept.mime.as_str(),kept.data_base64.as_str()),("agent-image.png","image/png","cG5n"));
        // a full person's quota leaves agent images alone, and the reverse
        store.conn().execute("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<4096) INSERT INTO sfp_uploads SELECT ?1,'filled-'||n,'empty','text/plain',x'' FROM ids",[&session.id]).unwrap();
        assert!(keep_agent_media(&store,&session.id,vec![media("b")]).unwrap().is_empty());
        store.conn().execute("DELETE FROM sfp_uploads WHERE upload_id LIKE 'filled-%'",[]).unwrap();
        store.conn().execute("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<4094) INSERT INTO sfp_uploads SELECT ?1,'sill-agent-filled-'||n,'empty','image/png',x'' FROM ids",[&session.id]).unwrap();
        let refused=keep_agent_media(&store,&session.id,vec![media("a"),media("c")]).unwrap();
        assert_eq!(refused.into_iter().collect::<Vec<_>>(),vec![format!("{AGENT_PREFIX}c")],"a kept image stays kept; a new one is refused");
        let person=UploadCreate{session_id:session.id.clone(),upload_id:"mine".into(),name:"notes.txt".into(),mime:"text/plain".into(),data_base64:"YQ==".into()};
        create(&store,person.clone()).unwrap();
        let mut forged=person;forged.upload_id=format!("{AGENT_PREFIX}x");
        assert!(create(&store,forged).is_err(),"clients cannot create agent media");
        drop(store);std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn base64_decoding_enforces_padding_and_decoded_bounds() {
        for bytes in [b"".as_slice(),b"a",b"ab",b"abc",b"hello world"] { assert_eq!(decode(&acp::base64(bytes)).unwrap(),bytes); }
        for bad in ["a", "=AAA", "AA=A", "AB==", "AAB=", "AA==AAAA", "AA\n="] { assert!(decode(bad).is_err(),"{bad}"); }
        assert_eq!(decode(&acp::base64(&vec![0;MAX_UPLOAD_BYTES])).unwrap().len(), MAX_UPLOAD_BYTES);
        assert!(decode(&acp::base64(&vec![0;MAX_UPLOAD_BYTES+1])).is_err());
    }
}
